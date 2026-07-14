#!/usr/bin/env bash
# Scenario: build the same git repo from two distinct on-disk paths
# (one via the original checkout, one via `git worktree add`), with a
# single shared zccache daemon. Path-remap must rewrite absolute
# source paths inside compiled artifacts so the second build hits.
#
# Pinned by issue #352 (ZCCACHE_PATH_REMAP=auto, Tier L1.x).
#
# Usage: run.sh <fixture-workdir>
set -euo pipefail

if (( $# != 1 )); then
    echo "usage: run.sh <fixture-workdir>" >&2
    exit 2
fi

FIXTURE_DIR="$1"
SCENARIO="worktree-share"

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../lib/common.sh
. "${HERE}/../../lib/common.sh"

WORKDIR="$(cd -- "${FIXTURE_DIR}/.." && pwd)"
CACHE="${WORKDIR}/cache-worktree"
RSS_CSV="${WORKDIR}/rss-${SCENARIO}.csv"
WORKTREE_B="${WORKDIR}/medium-worktree-b"

mkdir -p "${CACHE}"
measure::infrastructure_guard_init

# soldr's path-remap (#352) requires a real `.git/` checkout — tarball
# checkouts silently fall back to no remap. `git init` + one commit
# turns the fixture into a valid source for `git worktree add`.
(
    cd "${FIXTURE_DIR}"
    git init -q
    git -c user.email=perf@soldr.invalid -c user.name=perf \
        add . >/dev/null
    git -c user.email=perf@soldr.invalid -c user.name=perf \
        commit -q -m "perf fixture initial commit" >/dev/null
    git worktree add -q "${WORKTREE_B}" HEAD
)

measure::start_rss_poller "${RSS_CSV}"
trap 'measure::stop_rss_poller' EXIT

# --- Build worktree A (cold, populates cache) -----------------------

a_start_ms="$(measure::now_ms)"
pushd "${FIXTURE_DIR}" >/dev/null
if SOLDR_CACHE_DIR="${CACHE}" measure::run_guarded_soldr_command \
    "${CACHE}" "${WORKDIR}/soldr-aborts-a.jsonl" "worktree A build" \
    soldr cargo build --release; then
    :
else
    command_status=$?
    measure::emit_infrastructure_failure_json "${SCENARIO}" "${command_status}" \
        || echo "failed to emit infrastructure failure JSON" >&2
    exit "${command_status}"
fi
popd >/dev/null
a_elapsed_ms="$(measure::elapsed_ms "${a_start_ms}")"

SOLDR_CACHE_DIR="${CACHE}" soldr cache flush --json >/dev/null 2>&1 || true
SOLDR_CACHE_DIR="${CACHE}" soldr cache report --json \
    > "${WORKDIR}/a-cache-report.json" 2>/dev/null || true

cache_after_a_bytes="$(measure::cache_bytes "${CACHE}")"

# --- Build worktree B (should hit because path-remap rewrites src paths) ---

b_start_ms="$(measure::now_ms)"
pushd "${WORKTREE_B}" >/dev/null
if SOLDR_CACHE_DIR="${CACHE}" measure::run_guarded_soldr_command \
    "${CACHE}" "${WORKDIR}/soldr-aborts-b.jsonl" "worktree B build" \
    soldr cargo build --release; then
    :
else
    command_status=$?
    measure::emit_infrastructure_failure_json "${SCENARIO}" "${command_status}" \
        || echo "failed to emit infrastructure failure JSON" >&2
    exit "${command_status}"
fi
popd >/dev/null
b_elapsed_ms="$(measure::elapsed_ms "${b_start_ms}")"

b_stats="$(SOLDR_CACHE_DIR="${CACHE}" measure::session_end_json)"
b_hits="$(echo "${b_stats}" | jq -r '.stats.hits // 0')"
b_misses="$(echo "${b_stats}" | jq -r '.stats.misses // 0')"
b_hit_rate="$(echo "${b_stats}" | jq -r '.stats.hit_rate // 0')"

SOLDR_CACHE_DIR="${CACHE}" soldr cache report --json \
    > "${WORKDIR}/b-cache-report.json" 2>/dev/null || true

SOLDR_CACHE_DIR="${CACHE}" soldr cache shutdown \
    --shutdown-timeout-seconds 30 --json >"${WORKDIR}/worktree-shutdown.json" || true

cache_after_b_bytes="$(measure::cache_bytes "${CACHE}")"

# --- Measurement teardown ------------------------------------------

measure::stop_rss_poller
trap - EXIT

peak_daemon_rss="$(measure::peak_daemon_rss_bytes "${RSS_CSV}")"
peak_compile_rss="$(measure::peak_compile_rss_bytes "${RSS_CSV}")"

# A passing worktree-share row: cache_after_b_bytes ~= cache_after_a_bytes
# (no duplication) AND b_hit_rate close to 1.0. Bloat is the silent
# failure mode here, hence the explicit growth ratio.
if (( cache_after_a_bytes > 0 )); then
    growth_ratio="$(awk -v a="${cache_after_a_bytes}" -v b="${cache_after_b_bytes}" 'BEGIN{printf "%.4f", b/a}')"
else
    growth_ratio="0"
fi

# Speedup = a (cold) / b (warm-equivalent) (Nx). Guard against 0ms b.
if (( b_elapsed_ms > 0 )); then
    speedup="$(awk -v a="${a_elapsed_ms}" -v b="${b_elapsed_ms}" 'BEGIN { printf "%.2f", a / b }')"
else
    speedup="0.00"
fi

measure::emit_summary_json "${SCENARIO}" \
    "a_ms=${a_elapsed_ms}" \
    "b_ms=${b_elapsed_ms}" \
    "speedup=${speedup}" \
    "b_hits=${b_hits}" \
    "b_misses=${b_misses}" \
    "b_hit_rate=${b_hit_rate}" \
    "cache_after_a_bytes=${cache_after_a_bytes}" \
    "cache_after_b_bytes=${cache_after_b_bytes}" \
    "growth_ratio=${growth_ratio}" \
    "peak_daemon_rss_bytes=${peak_daemon_rss}" \
    "peak_compile_rss_bytes=${peak_compile_rss}" \
    "infrastructure_valid=${_MEASURE_INFRASTRUCTURE_VALID}" \
    "invalid_reasons=json:${_MEASURE_INVALID_REASONS_JSON}" \
    "soldr_abort_count=${_MEASURE_SOLDR_ABORT_COUNT}" \
    "soldr_timeout_count=${_MEASURE_SOLDR_TIMEOUT_COUNT}" \
    "soldr_no_cache_retry_count=${_MEASURE_SOLDR_NO_CACHE_RETRY_COUNT}" \
    "soldr_daemon_fallback_count=${_MEASURE_SOLDR_DAEMON_FALLBACK_COUNT}" \
    "soldr_abort_evidence=json:${_MEASURE_ABORT_EVIDENCE_JSON}" \
    "soldr_daemon_fallback_evidence=json:${_MEASURE_DAEMON_FALLBACK_EVIDENCE_JSON}"

measure::append_summary_md "| ${SCENARIO} | ${a_elapsed_ms} ms | ${b_elapsed_ms} ms | ${speedup}x | ${b_hits}/${b_misses} | ${b_hit_rate} | $(( peak_daemon_rss / 1024 / 1024 )) MiB |"
measure::fail_if_infrastructure_invalid

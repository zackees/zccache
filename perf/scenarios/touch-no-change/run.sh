#!/usr/bin/env bash
# Scenario: cold build, advance every source-file mtime (simulating
# a tarball restore from a CI cache where mtimes are fresh but content
# is unchanged), rebuild. zccache's content-hash fingerprint must
# defeat cargo's mtime-based freshness so every unit hits.
#
# Pinned by issue #377 (soldr save/load — content-verified mtimes).
#
# Usage: run.sh <fixture-workdir>
set -euo pipefail

if (( $# != 1 )); then
    echo "usage: run.sh <fixture-workdir>" >&2
    exit 2
fi

FIXTURE_DIR="$1"
SCENARIO="touch-no-change"

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../lib/common.sh
. "${HERE}/../../lib/common.sh"

WORKDIR="$(cd -- "${FIXTURE_DIR}/.." && pwd)"
CACHE="${WORKDIR}/cache-touch"
RSS_CSV="${WORKDIR}/rss-${SCENARIO}.csv"

mkdir -p "${CACHE}"
measure::infrastructure_guard_init

measure::start_rss_poller "${RSS_CSV}"
trap 'measure::stop_rss_poller' EXIT

# --- Cold build ----------------------------------------------------

cold_start_ms="$(measure::now_ms)"
pushd "${FIXTURE_DIR}" >/dev/null
if SOLDR_CACHE_DIR="${CACHE}" measure::run_guarded_soldr_command \
    "${CACHE}" "${WORKDIR}/soldr-aborts-cold.jsonl" "cold build" \
    soldr cargo build --release; then
    :
else
    command_status=$?
    measure::emit_infrastructure_failure_json "${SCENARIO}" "${command_status}" \
        || echo "failed to emit infrastructure failure JSON" >&2
    exit "${command_status}"
fi
popd >/dev/null
cold_elapsed_ms="$(measure::elapsed_ms "${cold_start_ms}")"

SOLDR_CACHE_DIR="${CACHE}" soldr cache flush --json >/dev/null 2>&1 || true
SOLDR_CACHE_DIR="${CACHE}" soldr cache report --json \
    > "${WORKDIR}/cold-cache-report.json" 2>/dev/null || true

# --- Touch every source file without changing content --------------

find "${FIXTURE_DIR}" -name '*.rs' -exec touch {} +
find "${FIXTURE_DIR}" -name 'Cargo.toml' -exec touch {} +
find "${FIXTURE_DIR}" -name 'Cargo.lock' -exec touch {} +

# Force cargo to re-evaluate freshness. Without `cargo clean` cargo
# might keep its incremental state and never ask the wrapper at all
# — we want zccache to be exercised, not bypassed.
(cd "${FIXTURE_DIR}" && cargo clean >/dev/null 2>&1)

# --- Warm build (should hit ~100% because content is unchanged) ----

warm_start_ms="$(measure::now_ms)"
pushd "${FIXTURE_DIR}" >/dev/null
if SOLDR_CACHE_DIR="${CACHE}" measure::run_guarded_soldr_command \
    "${CACHE}" "${WORKDIR}/soldr-aborts-warm.jsonl" "warm build" \
    soldr cargo build --release; then
    :
else
    command_status=$?
    measure::emit_infrastructure_failure_json "${SCENARIO}" "${command_status}" \
        || echo "failed to emit infrastructure failure JSON" >&2
    exit "${command_status}"
fi
popd >/dev/null
warm_elapsed_ms="$(measure::elapsed_ms "${warm_start_ms}")"

warm_stats="$(SOLDR_CACHE_DIR="${CACHE}" measure::session_end_json)"
warm_hits="$(echo "${warm_stats}" | jq -r '.stats.hits // 0')"
warm_misses="$(echo "${warm_stats}" | jq -r '.stats.misses // 0')"
warm_hit_rate="$(echo "${warm_stats}" | jq -r '.stats.hit_rate // 0')"

SOLDR_CACHE_DIR="${CACHE}" soldr cache report --json \
    > "${WORKDIR}/warm-cache-report.json" 2>/dev/null || true

SOLDR_CACHE_DIR="${CACHE}" soldr cache shutdown \
    --shutdown-timeout-seconds 30 --json >"${WORKDIR}/touch-shutdown.json" || true

cache_bytes="$(measure::cache_bytes "${CACHE}")"

# --- Measurement teardown ------------------------------------------

measure::stop_rss_poller
trap - EXIT

peak_daemon_rss="$(measure::peak_daemon_rss_bytes "${RSS_CSV}")"
peak_compile_rss="$(measure::peak_compile_rss_bytes "${RSS_CSV}")"

# Speedup = cold / warm (Nx). Guard against 0ms warm.
if (( warm_elapsed_ms > 0 )); then
    speedup="$(awk -v c="${cold_elapsed_ms}" -v w="${warm_elapsed_ms}" 'BEGIN { printf "%.2f", c / w }')"
else
    speedup="0.00"
fi

measure::emit_summary_json "${SCENARIO}" \
    "cold_ms=${cold_elapsed_ms}" \
    "warm_ms=${warm_elapsed_ms}" \
    "speedup=${speedup}" \
    "warm_hits=${warm_hits}" \
    "warm_misses=${warm_misses}" \
    "warm_hit_rate=${warm_hit_rate}" \
    "cache_bytes=${cache_bytes}" \
    "peak_daemon_rss_bytes=${peak_daemon_rss}" \
    "peak_compile_rss_bytes=${peak_compile_rss}" \
    "infrastructure_valid=${_MEASURE_INFRASTRUCTURE_VALID}" \
    "invalid_reasons=json:${_MEASURE_INVALID_REASONS_JSON}" \
    "soldr_abort_count=${_MEASURE_SOLDR_ABORT_COUNT}" \
    "soldr_timeout_count=${_MEASURE_SOLDR_TIMEOUT_COUNT}" \
    "soldr_no_cache_retry_count=${_MEASURE_SOLDR_NO_CACHE_RETRY_COUNT}" \
    "soldr_abort_evidence=json:${_MEASURE_ABORT_EVIDENCE_JSON}"

measure::append_summary_md "| ${SCENARIO} | ${cold_elapsed_ms} ms | ${warm_elapsed_ms} ms | ${speedup}x | ${warm_hits}/${warm_misses} | ${warm_hit_rate} | $(( peak_daemon_rss / 1024 / 1024 )) MiB |"
measure::fail_if_infrastructure_invalid

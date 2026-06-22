#!/usr/bin/env bash
# Scenario: cold `cargo build --release`, then immediate `cargo check
# --release` against an unchanged source tree.
#
# Reproduces soldr#758 for zccache: after build fills the cache with
# metadata+link artifacts, check should be able to reuse the metadata
# subset. Today zccache keys split on rustc's `--emit`, so check misses.
#
# Usage: run.sh <fixture-workdir>
set -euo pipefail

if (( $# != 1 )); then
    echo "usage: run.sh <fixture-workdir>" >&2
    exit 2
fi

FIXTURE_DIR="$1"
SCENARIO="build-then-check"

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../lib/common.sh
. "${HERE}/../../lib/common.sh"

WORKDIR="$(cd -- "${FIXTURE_DIR}/.." && pwd)"
CACHE="${WORKDIR}/cache-build-then-check"
RSS_CSV="${WORKDIR}/rss-${SCENARIO}.csv"

mkdir -p "${CACHE}"

measure::start_rss_poller "${RSS_CSV}"
trap 'measure::stop_rss_poller' EXIT

# --- Cold build (populates zccache for the release profile) --------

cold_start_ms="$(measure::now_ms)"
(
    cd "${FIXTURE_DIR}"
    SOLDR_CACHE_DIR="${CACHE}" soldr cargo build --release
)
cold_elapsed_ms="$(measure::elapsed_ms "${cold_start_ms}")"

# Flush so the depgraph snapshot is durable before the cross-verb pass.
SOLDR_CACHE_DIR="${CACHE}" soldr cache flush --json >/dev/null 2>&1 || true

# Cargo can sometimes accept build's rmeta as fresh enough and skip rustc
# entirely. Touch metadata without changing contents so cargo asks rustc for
# each unit while zccache still sees identical source bytes.
find "${FIXTURE_DIR}" -name '*.rs' -exec touch {} +
find "${FIXTURE_DIR}" -name 'Cargo.toml' -exec touch {} +
find "${FIXTURE_DIR}" -name 'Cargo.lock' -exec touch {} +

# --- Cross-verb check pass -----------------------------------------

warm_start_ms="$(measure::now_ms)"
(
    cd "${FIXTURE_DIR}"
    SOLDR_CACHE_DIR="${CACHE}" soldr cargo check --release
)
warm_elapsed_ms="$(measure::elapsed_ms "${warm_start_ms}")"

SOLDR_CACHE_DIR="${CACHE}" soldr cache report --json \
    > "${WORKDIR}/warm-cache-report.json" 2>/dev/null || true
cp -R "${CACHE}/cache/zccache/logs" "${WORKDIR}/warm-zccache-logs" 2>/dev/null || true

WARM_STATS_FILE="${CACHE}/cache/zccache/logs/last-session-stats.json"
if [[ -s "${WARM_STATS_FILE}" ]]; then
    warm_hits="$(jq -r '.stats.hits // .hits // 0' "${WARM_STATS_FILE}")"
    warm_misses="$(jq -r '.stats.misses // .misses // 0' "${WARM_STATS_FILE}")"
    warm_hit_rate="$(jq -r '.stats.hit_rate // .hit_rate // 0' "${WARM_STATS_FILE}")"
    warm_stats_source="file"
else
    warm_stats="$(SOLDR_CACHE_DIR="${CACHE}" measure::session_end_json)"
    warm_hits="$(echo "${warm_stats}" | jq -r '.stats.hits // 0')"
    warm_misses="$(echo "${warm_stats}" | jq -r '.stats.misses // 0')"
    warm_hit_rate="$(echo "${warm_stats}" | jq -r '.stats.hit_rate // 0')"
    warm_stats_source="session-end"
fi

SOLDR_CACHE_DIR="${CACHE}" soldr cache shutdown \
    --shutdown-timeout-seconds 30 --json >"${WORKDIR}/build-then-check-shutdown.json" || true

cache_bytes="$(measure::cache_bytes "${CACHE}")"

# --- Measurement teardown ------------------------------------------

measure::stop_rss_poller
trap - EXIT

peak_daemon_rss="$(measure::peak_daemon_rss_bytes "${RSS_CSV}")"
peak_compile_rss="$(measure::peak_compile_rss_bytes "${RSS_CSV}")"

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
    "warm_stats_source=${warm_stats_source}" \
    "cache_bytes=${cache_bytes}" \
    "peak_daemon_rss_bytes=${peak_daemon_rss}" \
    "peak_compile_rss_bytes=${peak_compile_rss}"

measure::append_summary_md "| ${SCENARIO} | ${cold_elapsed_ms} ms | ${warm_elapsed_ms} ms | ${speedup}x | ${warm_hits}/${warm_misses} | ${warm_hit_rate} | $(( peak_daemon_rss / 1024 / 1024 )) MiB |"

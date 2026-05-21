# shellcheck shell=bash
# Common helpers for perf cluster workers. Source this file with
# `. "${LIB_DIR}/common.sh"` from a scenario script.
#
# Conventions
# -----------
# * Every function lives under a `measure::` namespace.
# * State (timestamps, PIDs, CSV paths) is kept in process-local
#   globals named `_MEASURE_*` so two callers in the same shell can
#   round-trip cleanly. Scenarios that fan out should source this
#   file in each subshell rather than share state.
# * Output for `$GITHUB_STEP_SUMMARY` is markdown; output for the
#   master aggregator is JSON on stdout.

# --- RSS sidecar ---------------------------------------------------

# measure::start_rss_poller <csv-path>
#
# Backgrounds a 1Hz `ps` loop that appends `epoch,pid,rss,vsz,comm`
# rows for every running zccache-daemon / rustc / cargo process. The
# poller PID is stashed so `measure::stop_rss_poller` can kill it.
measure::start_rss_poller() {
    local csv="$1"
    _MEASURE_RSS_CSV="${csv}"
    echo "epoch,pid,rss_kb,vsz_kb,comm" > "${csv}"
    (
        while true; do
            # `ps -A -o pid,rss,vsz,comm --no-headers` is GNU-flavoured;
            # ubuntu runners have it. macOS workers (v2+) will need a
            # different code path.
            local now
            now="$(date +%s)"
            ps -A -o pid=,rss=,vsz=,comm= 2>/dev/null \
                | awk -v t="${now}" '
                    $4 ~ /^(zccache-daemon|zccache|rustc|cargo|soldr)$/ {
                        printf "%s,%s,%s,%s,%s\n", t, $1, $2, $3, $4
                    }' \
                >> "${csv}" || true
            sleep 1
        done
    ) &
    _MEASURE_RSS_PID="$!"
    # Detach so the poller survives `set -e` traps in the parent.
    disown "${_MEASURE_RSS_PID}" 2>/dev/null || true
}

# measure::stop_rss_poller
#
# Kills the background poller started by `start_rss_poller`. Safe to
# call when no poller is running.
measure::stop_rss_poller() {
    if [[ -n "${_MEASURE_RSS_PID:-}" ]]; then
        kill "${_MEASURE_RSS_PID}" 2>/dev/null || true
        wait "${_MEASURE_RSS_PID}" 2>/dev/null || true
        _MEASURE_RSS_PID=""
    fi
}

# measure::peak_daemon_rss_bytes <csv-path>
#
# Prints the largest zccache-daemon RSS observed in the CSV (in
# bytes). Prints `0` if no daemon rows are present.
measure::peak_daemon_rss_bytes() {
    local csv="$1"
    awk -F, '
        NR == 1 { next }
        $5 == "zccache-daemon" || $5 == "zccache" {
            kb = $3 + 0
            if (kb > peak) peak = kb
        }
        END { print (peak ? peak : 0) * 1024 }
    ' "${csv}"
}

# measure::peak_compile_rss_bytes <csv-path>
#
# Peak rustc + cargo RSS seen across the whole CSV.
measure::peak_compile_rss_bytes() {
    local csv="$1"
    awk -F, '
        NR == 1 { next }
        $5 == "rustc" || $5 == "cargo" {
            kb = $3 + 0
            if (kb > peak) peak = kb
        }
        END { print (peak ? peak : 0) * 1024 }
    ' "${csv}"
}

# --- Disk footprint -------------------------------------------------

# measure::cache_bytes <cache-root>
#
# Total bytes under <cache-root>/cache/zccache. The standard soldr
# layout puts everything cache-related there; the scenario points
# $SOLDR_CACHE_DIR at the parent so the same path resolves on disk.
measure::cache_bytes() {
    local cache_root="$1"
    local zccache_dir="${cache_root}/cache/zccache"
    if [[ -d "${zccache_dir}" ]]; then
        du -sb "${zccache_dir}" | awk '{print $1}'
    else
        echo 0
    fi
}

# --- Soldr stats wrappers -------------------------------------------

# measure::session_end_json <session-id-or-empty>
#
# Run `soldr session-end --json` and print the parsed JSON on stdout.
# When no session id is given soldr uses $ZCCACHE_SESSION_ID.
# Returns an empty object if the call fails (the scenario is still
# useful when, e.g., the daemon never started a session).
measure::session_end_json() {
    local id="${1:-}"
    local cmd=("soldr" "session-end" "--json")
    if [[ -n "${id}" ]]; then
        cmd+=("--id" "${id}")
    fi
    if out="$("${cmd[@]}" 2>/dev/null)"; then
        echo "${out}"
    else
        echo "{}"
    fi
}

# --- Wall-time --------------------------------------------------------

# measure::now_ms
measure::now_ms() {
    date +%s%3N
}

# measure::elapsed_ms <start-ms>
measure::elapsed_ms() {
    local start="$1"
    local now
    now="$(measure::now_ms)"
    echo $(( now - start ))
}

# --- Summary emission -----------------------------------------------

# measure::emit_summary_json <scenario> <key=value>...
#
# Prints a single JSON object on stdout with the provided key/value
# pairs (all values are emitted as strings unless they match a
# number-only regex, in which case they are emitted as JSON numbers).
# A `scenario` key is always included.
measure::emit_summary_json() {
    local scenario="$1"; shift
    local first=1
    printf '{"scenario":"%s"' "${scenario}"
    for kv in "$@"; do
        local key="${kv%%=*}"
        local value="${kv#*=}"
        printf ','
        if [[ "${value}" =~ ^-?[0-9]+(\.[0-9]+)?$ ]]; then
            printf '"%s":%s' "${key}" "${value}"
        else
            # Naive JSON-string escape: backslash + double quote.
            local escaped="${value//\\/\\\\}"
            escaped="${escaped//\"/\\\"}"
            printf '"%s":"%s"' "${key}" "${escaped}"
        fi
        first=0
    done
    printf '}\n'
}

# measure::append_summary_md <table-row>
#
# Append a single markdown row to $GITHUB_STEP_SUMMARY when running
# inside a GHA worker. No-op locally so scripts stay testable.
measure::append_summary_md() {
    if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
        echo "$1" >> "${GITHUB_STEP_SUMMARY}"
    fi
}

# measure::reset_cache_dir <cache-root>
#
# Wipe a soldr cache root so the next build starts cold. Stops the
# daemon first so we do not race the file system.
measure::reset_cache_dir() {
    local cache_root="$1"
    if command -v soldr >/dev/null 2>&1; then
        SOLDR_CACHE_DIR="${cache_root}" soldr cache shutdown \
            --shutdown-timeout-seconds 15 --json >/dev/null 2>&1 || true
    fi
    rm -rf "${cache_root}/cache" "${cache_root}/bin" 2>/dev/null || true
    mkdir -p "${cache_root}"
}

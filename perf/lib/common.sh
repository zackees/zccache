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
# Backgrounds a 1Hz process loop that appends `epoch,pid,rss,vsz,comm`
# rows for every running zccache-daemon / rustc / cargo process. The
# poller PID is stashed so `measure::stop_rss_poller` can kill it.
measure::start_rss_poller() {
    local csv="$1"
    _MEASURE_RSS_CSV="${csv}"
    echo "epoch,pid,rss_kb,vsz_kb,comm" > "${csv}"
    case "$(uname -s)" in
        MINGW*|MSYS*|CYGWIN*)
            (
                while true; do
                    # Feed the one-shot sample over stdin. A temporary .ps1
                    # can remain locked by powershell.exe after the Bash
                    # poller exits, making cleanup fail on Windows.
                    powershell.exe -NoLogo -NoProfile -NonInteractive \
                        -ExecutionPolicy Bypass -Command - \
                        >> "${csv}" 2>/dev/null <<'POWERSHELL' || true
$now = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
Get-Process | Where-Object {
    $_.ProcessName -match '^(zccache-daemon|zccache|rustc|cargo|soldr)(\.|$)'
} | ForEach-Object {
    $name = $_.ProcessName -replace '\..*$', ''
    '{0},{1},{2},{3},{4}' -f $now, $_.Id,
        [math]::Floor($_.WorkingSet64 / 1KB),
        [math]::Floor($_.VirtualMemorySize64 / 1KB), $name
}
POWERSHELL
                    sleep 1
                done
            ) &
            ;;
        *)
            (
                while true; do
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
            ;;
    esac
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
# Prints the largest embedded soldr or standalone zccache-daemon RSS observed
# in the CSV (in bytes). Prints `0` if no daemon rows are present.
measure::peak_daemon_rss_bytes() {
    local csv="$1"
    awk -F, '
        NR == 1 { next }
        $5 == "soldr" || $5 == "zccache-daemon" || $5 == "zccache" {
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
        du -sk "${zccache_dir}" | awk '{print $1 * 1024}'
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
    case "$(uname -s)" in
        MINGW*|MSYS*|CYGWIN*)
            powershell.exe -NoLogo -NoProfile -NonInteractive -Command \
                '[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()' | tr -d '\r'
            ;;
        Darwin)
            perl -MTime::HiRes=time -e 'printf "%.0f\n", time() * 1000'
            ;;
        *)
            date +%s%3N
            ;;
    esac
}

# measure::elapsed_ms <start-ms>
measure::elapsed_ms() {
    local start="$1"
    local now
    now="$(measure::now_ms)"
    echo $(( now - start ))
}

# --- Benchmark infrastructure validity ------------------------------

# Each soldr cache root owns its own append-only cargo-aborts.jsonl. A
# scenario snapshots the byte offset immediately before each measured command
# and copies only newly appended records into its artifact directory. This
# keeps stale records and concurrently running scenarios out of the verdict.
measure::infrastructure_guard_init() {
    _MEASURE_INFRASTRUCTURE_VALID=true
    _MEASURE_INVALID_REASONS_JSON='[]'
    _MEASURE_SOLDR_ABORT_COUNT=0
    _MEASURE_SOLDR_TIMEOUT_COUNT=0
    _MEASURE_SOLDR_NO_CACHE_RETRY_COUNT=0
    _MEASURE_ABORT_EVIDENCE_JSON='[]'
}

measure::soldr_abort_log_bytes() {
    local log="${1}/logs/cargo-aborts.jsonl"
    if [[ -f "${log}" ]]; then
        wc -c < "${log}" | tr -d '[:space:]'
    else
        echo 0
    fi
}

measure::soldr_abort_log_prefix_fingerprint() {
    local log="$1"
    local bytes="$2"
    if (( bytes == 0 )); then
        echo "0:0"
    else
        head -c "${bytes}" "${log}" | cksum | awk '{ print $1 ":" $2 }'
    fi
}

measure::_mark_infrastructure_invalid() {
    local reason="$1"
    _MEASURE_INFRASTRUCTURE_VALID=false
    _MEASURE_INVALID_REASONS_JSON="$(
        jq -cn --argjson reasons "${_MEASURE_INVALID_REASONS_JSON}" \
            --arg reason "${reason}" '$reasons + [$reason]'
    )"
}

# measure::capture_soldr_abort_delta <cache-root> <baseline-bytes>
#                                    <baseline-fingerprint> <evidence-file>
#                                    <phase-label>
measure::capture_soldr_abort_delta() {
    local cache_root="$1"
    local baseline_bytes="$2"
    local baseline_fingerprint="$3"
    local evidence_file="$4"
    local phase="$5"
    local log="${cache_root}/logs/cargo-aborts.jsonl"
    local evidence_name current_bytes current_fingerprint delta_bytes captured_bytes line
    local updated_evidence_paths
    local aborts=0 timeouts=0 retries=0 malformed=0

    evidence_name="$(basename -- "${evidence_file}")"
    if ! mkdir -p "$(dirname -- "${evidence_file}")"; then
        echo "${phase}: could not create soldr abort evidence directory" >&2
        return 1
    fi
    if ! : > "${evidence_file}"; then
        echo "${phase}: could not create soldr abort evidence file ${evidence_file}" >&2
        return 1
    fi
    if ! updated_evidence_paths="$(
        jq -cn --argjson paths "${_MEASURE_ABORT_EVIDENCE_JSON}" \
            --arg path "${evidence_name}" '$paths + [$path]'
    )"; then
        echo "${phase}: could not update soldr abort evidence metadata" >&2
        return 1
    fi
    _MEASURE_ABORT_EVIDENCE_JSON="${updated_evidence_paths}"

    if ! current_bytes="$(measure::soldr_abort_log_bytes "${cache_root}")"; then
        echo "${phase}: could not read soldr abort log size" >&2
        return 1
    fi
    if (( current_bytes < baseline_bytes )); then
        cp "${log}" "${evidence_file}" 2>/dev/null || true
        measure::_mark_infrastructure_invalid \
            "${phase}: soldr cargo-aborts.jsonl was truncated during the measured command; evidence=${evidence_name}"
        return 0
    fi

    if ! current_fingerprint="$(
        measure::soldr_abort_log_prefix_fingerprint "${log}" "${baseline_bytes}"
    )"; then
        echo "${phase}: could not fingerprint soldr abort log" >&2
        return 1
    fi
    if [[ "${current_fingerprint}" != "${baseline_fingerprint}" ]]; then
        cp "${log}" "${evidence_file}" 2>/dev/null || true
        measure::_mark_infrastructure_invalid \
            "${phase}: soldr cargo-aborts.jsonl existing prefix changed during the measured command; evidence=${evidence_name}"
        return 0
    fi
    if (( current_bytes == baseline_bytes )); then
        return 0
    fi

    # Bound the copy to the size observed above. A later asynchronous append
    # belongs to the next command and must not turn this snapshot into a false
    # partial-record failure.
    delta_bytes=$(( current_bytes - baseline_bytes ))
    if ! head -c "${current_bytes}" "${log}" \
        | tail -c "${delta_bytes}" > "${evidence_file}"; then
        echo "${phase}: failed to copy soldr abort evidence from ${log}" >&2
        return 1
    fi
    captured_bytes="$(wc -c < "${evidence_file}" | tr -d '[:space:]')"
    if (( captured_bytes != delta_bytes )); then
        echo "${phase}: soldr abort evidence changed while being copied" >&2
        return 1
    fi

    while IFS= read -r line || [[ -n "${line}" ]]; do
        [[ -z "${line}" ]] && continue
        if ! jq -e 'type == "object" and .event == "cargo_abort"' \
            >/dev/null 2>&1 <<<"${line}"; then
            malformed=$(( malformed + 1 ))
            continue
        fi
        aborts=$(( aborts + 1 ))
        if jq -e '.timeout == true' >/dev/null 2>&1 <<<"${line}"; then
            timeouts=$(( timeouts + 1 ))
        fi
        if jq -e '.auto_retry_planned == true' >/dev/null 2>&1 <<<"${line}"; then
            retries=$(( retries + 1 ))
        fi
    done < "${evidence_file}"

    _MEASURE_SOLDR_ABORT_COUNT=$(( _MEASURE_SOLDR_ABORT_COUNT + aborts ))
    _MEASURE_SOLDR_TIMEOUT_COUNT=$(( _MEASURE_SOLDR_TIMEOUT_COUNT + timeouts ))
    _MEASURE_SOLDR_NO_CACHE_RETRY_COUNT=$(( _MEASURE_SOLDR_NO_CACHE_RETRY_COUNT + retries ))

    if (( malformed > 0 )); then
        measure::_mark_infrastructure_invalid \
            "${phase}: ${malformed} malformed or partial soldr abort record(s); evidence=${evidence_name}"
    fi
    if (( aborts > 0 )); then
        measure::_mark_infrastructure_invalid \
            "${phase}: soldr recorded ${aborts} cargo abort(s), ${timeouts} timeout(s), and ${retries} no-cache retry plan(s); evidence=${evidence_name}"
    fi
}

# measure::run_guarded_soldr_command <cache-root> <evidence-file>
#                                    <phase-label> <command> [args...]
measure::run_guarded_soldr_command() {
    local cache_root="$1"
    local evidence_file="$2"
    local phase="$3"
    shift 3
    local log baseline_bytes baseline_fingerprint command_status capture_status=0

    log="${cache_root}/logs/cargo-aborts.jsonl"
    if ! baseline_bytes="$(measure::soldr_abort_log_bytes "${cache_root}")"; then
        measure::_mark_infrastructure_invalid \
            "${phase}: could not read initial soldr abort log size; evidence=$(basename -- "${evidence_file}")"
        return 1
    fi
    if (( baseline_bytes > 0 )); then
        if ! baseline_fingerprint="$(
            measure::soldr_abort_log_prefix_fingerprint "${log}" "${baseline_bytes}"
        )"; then
            measure::_mark_infrastructure_invalid \
                "${phase}: could not fingerprint initial soldr abort log; evidence=$(basename -- "${evidence_file}")"
            return 1
        fi
    else
        baseline_fingerprint="0:0"
    fi
    if "$@"; then
        command_status=0
    else
        command_status=$?
    fi
    if measure::capture_soldr_abort_delta \
        "${cache_root}" "${baseline_bytes}" "${baseline_fingerprint}" \
        "${evidence_file}" "${phase}"; then
        capture_status=0
    else
        capture_status=$?
        measure::_mark_infrastructure_invalid \
            "${phase}: failed to capture soldr abort evidence; evidence=$(basename -- "${evidence_file}")"
    fi
    if (( command_status != 0 )); then
        measure::_mark_infrastructure_invalid \
            "${phase}: measured command exited with status ${command_status}; evidence=$(basename -- "${evidence_file}")"
    fi
    if (( command_status != 0 )); then
        return "${command_status}"
    fi
    return "${capture_status}"
}

measure::emit_infrastructure_failure_json() {
    local scenario="$1"
    local guard_status="$2"
    measure::emit_summary_json "${scenario}" \
        "infrastructure_valid=${_MEASURE_INFRASTRUCTURE_VALID}" \
        "invalid_reasons=json:${_MEASURE_INVALID_REASONS_JSON}" \
        "soldr_abort_count=${_MEASURE_SOLDR_ABORT_COUNT}" \
        "soldr_timeout_count=${_MEASURE_SOLDR_TIMEOUT_COUNT}" \
        "soldr_no_cache_retry_count=${_MEASURE_SOLDR_NO_CACHE_RETRY_COUNT}" \
        "soldr_abort_evidence=json:${_MEASURE_ABORT_EVIDENCE_JSON}" \
        "guarded_command_status=${guard_status}"
}

measure::fail_if_infrastructure_invalid() {
    if [[ "${_MEASURE_INFRASTRUCTURE_VALID:-false}" != "true" ]]; then
        echo "benchmark infrastructure invalid: ${_MEASURE_INVALID_REASONS_JSON:-[]}" >&2
        return 1
    fi
}

# --- Summary emission -----------------------------------------------

# measure::emit_summary_json <scenario> <key=value>...
#
# Prints a single JSON object on stdout with the provided key/value
# pairs. Numbers and booleans are emitted as their native JSON types;
# `json:<value>` emits a pre-validated object or array. Everything else is a
# string. A `scenario` key is always included.
measure::emit_summary_json() {
    local scenario="$1"; shift
    local first=1
    printf '{"scenario":"%s"' "${scenario}"
    for kv in "$@"; do
        local key="${kv%%=*}"
        local value="${kv#*=}"
        printf ','
        if [[ "${value}" =~ ^-?[0-9]+(\.[0-9]+)?$ ]] \
            || [[ "${value}" == "true" || "${value}" == "false" ]]; then
            printf '"%s":%s' "${key}" "${value}"
        elif [[ "${value}" == json:* ]]; then
            local raw_json="${value#json:}"
            if ! jq -e . >/dev/null 2>&1 <<<"${raw_json}"; then
                echo "invalid raw JSON for summary key ${key}" >&2
                return 1
            fi
            printf '"%s":%s' "${key}" "${raw_json}"
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

# measure::quiesce_cache_for_snapshot <cache-root> <shutdown-report>
#
# Flush embedded zccache state and stop the owning soldr-daemon before
# archiving a cache tree.  A flush alone is insufficient on Windows: the
# still-live broker retains NTFS byte-range locks on SQLite files, causing
# `soldr save` to fail (or, worse, making an external copier race a writer).
measure::quiesce_cache_for_snapshot() {
    local cache_root="$1"
    local shutdown_report="$2"
    local status_json pid="" deadline

    SOLDR_CACHE_DIR="${cache_root}" soldr cache flush --json >/dev/null
    SOLDR_CACHE_DIR="${cache_root}" soldr cache shutdown \
        --shutdown-timeout-seconds 30 --json >"${shutdown_report}"
    if ! jq -e '.daemon_stopped == true' "${shutdown_report}" >/dev/null; then
        echo "cache snapshot refused: embedded zccache flush was not confirmed" >&2
        return 1
    fi

    status_json="$(SOLDR_CACHE_DIR="${cache_root}" soldr daemon status --json)"
    if jq -e '.running == true' >/dev/null <<<"${status_json}"; then
        pid="$(jq -r '.pid' <<<"${status_json}")"
    fi
    SOLDR_CACHE_DIR="${cache_root}" soldr daemon stop >/dev/null

    deadline=$(( SECONDS + 30 ))
    while (( SECONDS < deadline )); do
        status_json="$(SOLDR_CACHE_DIR="${cache_root}" soldr daemon status --json 2>/dev/null || true)"
        if jq -e '.running == false' >/dev/null 2>&1 <<<"${status_json}"; then
            if [[ -z "${pid}" ]] || ! measure::_pid_is_alive "${pid}"; then
                return 0
            fi
        fi
        sleep 0.2
    done

    echo "cache snapshot refused: soldr-daemon did not exit within 30 seconds" >&2
    return 1
}

measure::_pid_is_alive() {
    local pid="$1"
    case "$(uname -s)" in
        MINGW*|MSYS*|CYGWIN*)
            powershell.exe -NoProfile -NonInteractive -Command \
                "if (Get-Process -Id ${pid} -ErrorAction SilentlyContinue) { exit 0 } else { exit 1 }" \
                >/dev/null 2>&1
            ;;
        *)
            kill -0 "${pid}" >/dev/null 2>&1
            ;;
    esac
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

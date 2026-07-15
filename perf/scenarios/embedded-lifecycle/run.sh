#!/usr/bin/env bash

set -euo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../lib/common.sh
. "${HERE}/../../lib/common.sh"

: "${EMBEDDED_LANGUAGE:?}"
: "${EMBEDDED_WORK_ROOT:?}"
: "${EMBEDDED_FIXTURE:?}"
: "${EMBEDDED_TOOL_VERSIONS:?}"
: "${FIXTURE_SHA256:?}"
: "${SOLDR_SHA:?}"
: "${ZCCACHE_SHA:?}"
: "${IMAGE_DIGEST:?}"
: "${HOST_FINGERPRINT:?}"

RESULTS=/results
CACHE="${EMBEDDED_WORK_ROOT}/cache"
REPO_A="${EMBEDDED_FIXTURE}"
REPO_B="${EMBEDDED_WORK_ROOT}/repo-b"
RSS_CSV="${RESULTS}/rss.csv"
PHASES_DIR="${RESULTS}/phases"
mkdir -p "${CACHE}" "${PHASES_DIR}"

case "${EMBEDDED_LANGUAGE}" in
    rust)
        COMPILER_ENV=()
        ;;
    c)
        COMPILER_ENV=("CC=/usr/local/bin/clang")
        ;;
    cpp)
        COMPILER_ENV=("CXX=/usr/local/bin/clang++")
        ;;
    emscripten)
        COMPILER_ENV=("CXX=/emsdk/upstream/emscripten/em++")
        ;;
    *)
        echo "unsupported embedded language: ${EMBEDDED_LANGUAGE}" >&2
        exit 2
        ;;
esac

(
    cd "${REPO_A}"
    git init -q
    git -c user.email=perf@zccache.invalid -c user.name=perf add .
    git -c user.email=perf@zccache.invalid -c user.name=perf \
        commit -q -m "embedded perf fixture"
    git worktree add -q "${REPO_B}" HEAD
)

measure::infrastructure_guard_init
measure::start_rss_poller "${RSS_CSV}"
(
    while true; do
        now="$(date +%s)"
        ps -A -o pid=,rss=,vsz=,comm= 2>/dev/null \
            | awk -v t="${now}" '
                $4 ~ /^(soldr-daemon|clang|clang-[0-9]+|clang\+\+|emcc|em\+\+)$/ {
                    printf "%s,%s,%s,%s,%s\n", t, $1, $2, $3, $4
                }' >>"${RSS_CSV}" || true
        sleep 0.1
    done
) &
EXTENDED_RSS_PID=$!
trap 'measure::stop_rss_poller; kill "${EXTENDED_RSS_PID}" 2>/dev/null || true' EXIT

TIMED_WALL_MS=0
TIMED_USER_MS=0
TIMED_SYSTEM_MS=0
TIMED_TTFB_MS=0
TIMED_OUTPUT_BYTES=0
TIMED_MAX_RSS_BYTES=0

timed_command() {
    local log="$1" resources="$2" working_directory="$3"
    shift 3
    local first_output="${resources}.first-ms" fifo="${resources}.fifo"
    local reader_pid start status
    rm -f "${first_output}" "${fifo}"
    mkfifo "${fifo}"
    start="$(measure::now_ms)"
    (
        while IFS= read -r line || [[ -n "${line}" ]]; do
            if [[ ! -e "${first_output}" ]]; then
                measure::now_ms >"${first_output}"
            fi
            printf '%s\n' "${line}"
        done <"${fifo}" >"${log}"
    ) &
    reader_pid=$!
    set +e
    (
        cd "${working_directory}"
        /usr/bin/time -f 'user_seconds=%U\nsystem_seconds=%S\nmax_rss_kb=%M' -o "${resources}" \
            "$@" >"${fifo}" 2>&1
    )
    status=$?
    wait "${reader_pid}"
    set -e
    TIMED_WALL_MS="$(measure::elapsed_ms "${start}")"
    TIMED_USER_MS="$(awk -F= '/^user_seconds=/{printf "%.0f", $2 * 1000}' "${resources}")"
    TIMED_SYSTEM_MS="$(awk -F= '/^system_seconds=/{printf "%.0f", $2 * 1000}' "${resources}")"
    TIMED_MAX_RSS_BYTES="$(awk -F= '/^max_rss_kb=/{printf "%.0f", $2 * 1024}' "${resources}")"
    TIMED_OUTPUT_BYTES="$(wc -c <"${log}" | tr -d '[:space:]')"
    if [[ -s "${first_output}" ]]; then
        TIMED_TTFB_MS=$(( $(<"${first_output}") - start ))
    else
        TIMED_TTFB_MS="${TIMED_WALL_MS}"
    fi
    rm -f "${first_output}" "${fifo}"
    return "${status}"
}

artifact_manifest() {
    local repo="$1" destination="$2"
    local target rust_binary native_dir
    local replay compiler_command artifact_bytes
    local -a files=()

    target="${repo}/target"
    rust_binary="${target}/release/embedded-fixture"
    native_dir="${target}/embedded-artifacts/${EMBEDDED_LANGUAGE}"
    files=("${rust_binary}")

    [[ -x "${rust_binary}" ]] || { echo "missing Rust artifact ${rust_binary}" >&2; return 1; }
    replay="$("${rust_binary}")"
    [[ "${replay}" == "embedded-fixture-ok:${EMBEDDED_LANGUAGE}" ]] || return 1
    compiler_command="rustc"

    if [[ "${EMBEDDED_LANGUAGE}" != rust ]]; then
        [[ -s "${native_dir}/compiler-command.txt" ]] || return 1
        compiler_command="$(<"${native_dir}/compiler-command.txt")"
        [[ "${compiler_command}" == *zccache-soldr* ]] || {
            echo "native compiler bypassed zccache-soldr: ${compiler_command}" >&2
            return 1
        }
        for object in unit_a.o unit_b.o main.o; do
            [[ -s "${native_dir}/${object}" ]] || return 1
            files+=("${native_dir}/${object}")
        done
        if [[ "${EMBEDDED_LANGUAGE}" == emscripten ]]; then
            [[ -s "${native_dir}/app.js" && -s "${native_dir}/app.wasm" ]] || return 1
            [[ "$(head -c 4 "${native_dir}/app.wasm" | od -An -tx1 | tr -d ' \n')" == "0061736d" ]] || return 1
            replay="$(node "${native_dir}/app.js")"
            files+=("${native_dir}/app.js" "${native_dir}/app.wasm")
        else
            [[ -x "${native_dir}/app" ]] || return 1
            replay="$("${native_dir}/app")"
            files+=("${native_dir}/app")
        fi
        [[ "${replay}" == "42" ]] || return 1
    fi

    printf '%s\n' "${compiler_command}" >"${RESULTS}/compiler-command.txt"
    printf '%s\0' "${files[@]}" \
        | xargs -0 sha256sum \
        | sed "s#${repo}/##" \
        | sort >"${destination}.hashes"
    artifact_bytes="$(stat -c '%s' "${files[@]}" | awk '{sum += $1} END {print sum + 0}')"
    jq -Rn \
        --arg language "${EMBEDDED_LANGUAGE}" \
        --arg replay "${replay}" \
        --argjson artifact_bytes "${artifact_bytes}" \
        --rawfile hashes "${destination}.hashes" \
        '{language: $language, replay: $replay, artifact_bytes: $artifact_bytes,
          sha256_lines: ($hashes | split("\n") | map(select(length > 0)))}' \
        >"${destination}"
    rm "${destination}.hashes"
}

write_phase() {
    local phase="$1" repo="$2" command_json="$3" no_op="${4:-false}"
    local prefix="${PHASES_DIR}/${phase}"
    local report="${prefix}-cache-report.json"
    local manifest="${prefix}-outputs.json"
    local cache_bytes artifact_count artifact_bytes artifact_sha
    local compilations hits misses non_cacheable phase_profile

    SOLDR_CACHE_DIR="${CACHE}" soldr cache report --json >"${report}"
    artifact_manifest "${repo}" "${manifest}"
    artifact_sha="$(jq -r '.sha256_lines[]' "${manifest}" | sha256sum | awk '{print $1}')"
    cache_bytes="$(measure::cache_bytes "${CACHE}")"
    artifact_count="$(jq '.sha256_lines | length' "${manifest}")"
    artifact_bytes="$(jq '.artifact_bytes' "${manifest}")"
    if [[ "${no_op}" == true ]]; then
        compilations="$({
            jq -Rr 'fromjson? | select(.reason == "compiler-artifact" and .fresh == false) | 1' \
                "${prefix}.log" | wc -l | tr -d '[:space:]'
        })"
        hits=0 misses=0 non_cacheable=0 phase_profile='{}'
    else
        compilations="$(jq -r '.last_session.compilations // ((.last_session.hits // 0) + (.last_session.misses // 0) + (.last_session.non_cacheable // 0))' "${report}")"
        hits="$(jq -r '.last_session.hits // 0' "${report}")"
        misses="$(jq -r '.last_session.misses // 0' "${report}")"
        non_cacheable="$(jq -r '.last_session.non_cacheable // 0' "${report}")"
        phase_profile="$(jq -c '.last_session.phase_profile // {}' "${report}")"
    fi
    jq -n \
        --arg name "${phase}" \
        --argjson wall_ms "${TIMED_WALL_MS}" \
        --argjson user_cpu_ms "${TIMED_USER_MS:-0}" \
        --argjson system_cpu_ms "${TIMED_SYSTEM_MS:-0}" \
        --argjson ttfb_ms "${TIMED_TTFB_MS}" \
        --argjson output_bytes "${TIMED_OUTPUT_BYTES}" \
        --argjson peak_command_rss_bytes "${TIMED_MAX_RSS_BYTES:-0}" \
        --argjson cache_bytes "${cache_bytes}" \
        --argjson artifact_count "${artifact_count}" \
        --argjson artifact_bytes "${artifact_bytes}" \
        --arg artifact_sha256 "${artifact_sha}" \
        --argjson compilations "${compilations}" \
        --argjson hits "${hits}" \
        --argjson misses "${misses}" \
        --argjson non_cacheable "${non_cacheable}" \
        --argjson phase_profile "${phase_profile}" \
        --argjson command "${command_json}" \
        --arg working_directory "${repo}" \
        --arg command_log "phases/${phase}.log" \
        --arg resource_usage "phases/${phase}-resources.txt" \
        --arg cache_report "phases/${phase}-cache-report.json" \
        --arg output_manifest "phases/${phase}-outputs.json" \
        '{name: $name, wall_ms: $wall_ms, user_cpu_ms: $user_cpu_ms,
          system_cpu_ms: $system_cpu_ms, ttfb_ms: $ttfb_ms,
          output_bytes: $output_bytes, peak_command_rss_bytes: $peak_command_rss_bytes,
          cache_bytes: $cache_bytes,
          artifact_count: $artifact_count, artifact_bytes: $artifact_bytes,
          artifact_sha256: $artifact_sha256, compilations: $compilations,
          hits: $hits, misses: $misses, non_cacheable: $non_cacheable,
          phase_profile: $phase_profile, command: $command,
          working_directory: $working_directory,
          artifacts: {command_log: $command_log, resource_usage: $resource_usage,
            cache_report: $cache_report, output_manifest: $output_manifest}}' \
        >"${prefix}.json"
}

run_build_phase() {
    local phase="$1" repo="$2" no_op="${3:-false}"
    local prefix="${PHASES_DIR}/${phase}"
    local command_json
    command_json="$({
        jq -cn \
            --arg cache "${CACHE}" \
            --arg target "${repo}/target" \
            --arg language "${EMBEDDED_LANGUAGE}" \
            --arg compiler_env "${COMPILER_ENV[*]}" \
            '["env", "SOLDR_CACHE_DIR=" + $cache,
              "CARGO_TARGET_DIR=" + $target,
              "EMBEDDED_LANGUAGE=" + $language]
             + (if $compiler_env == "" then [] else [$compiler_env] end)
             + ["soldr", "cargo", "build", "--release", "--locked",
                "--message-format=json-render-diagnostics"]'
    })"
    if ! SOLDR_CACHE_DIR="${CACHE}" CARGO_TARGET_DIR="${repo}/target" \
        EMBEDDED_LANGUAGE="${EMBEDDED_LANGUAGE}" \
        measure::run_guarded_soldr_command \
            "${CACHE}" "${RESULTS}/soldr-aborts-${phase}.jsonl" "${phase}" \
            timed_command "${prefix}.log" "${prefix}-resources.txt" "${repo}" \
            env "${COMPILER_ENV[@]}" soldr cargo build --release --locked \
                --message-format=json-render-diagnostics; then
        echo "embedded ${phase} failed" >&2
        return 1
    fi
    write_phase "${phase}" "${repo}" "${command_json}" "${no_op}"
}

# First daemon startup is timed independently from the already-running cold build.
daemon_prefix="${PHASES_DIR}/daemon-start"
daemon_command="$(jq -cn --arg cache "${CACHE}" \
    '["env", "SOLDR_CACHE_DIR=" + $cache, "soldr", "daemon", "start"]')"
SOLDR_CACHE_DIR="${CACHE}" timed_command \
    "${daemon_prefix}.log" "${daemon_prefix}-resources.txt" /src \
    soldr daemon start
SOLDR_CACHE_DIR="${CACHE}" soldr cache report --json >"${daemon_prefix}-cache-report.json"
jq -n '{language: null, replay: "", sha256_lines: []}' >"${daemon_prefix}-outputs.json"
daemon_artifact_sha="$(printf '' | sha256sum | awk '{print $1}')"
jq -n \
    --argjson wall_ms "${TIMED_WALL_MS}" --argjson user_cpu_ms "${TIMED_USER_MS:-0}" \
    --argjson system_cpu_ms "${TIMED_SYSTEM_MS:-0}" --argjson ttfb_ms "${TIMED_TTFB_MS}" \
    --argjson output_bytes "${TIMED_OUTPUT_BYTES}" --argjson command "${daemon_command}" \
    --argjson peak_command_rss_bytes "${TIMED_MAX_RSS_BYTES:-0}" \
    --arg artifact_sha256 "${daemon_artifact_sha}" \
    '{name:"daemon-start", wall_ms:$wall_ms, user_cpu_ms:$user_cpu_ms,
      system_cpu_ms:$system_cpu_ms, ttfb_ms:$ttfb_ms, output_bytes:$output_bytes,
      peak_command_rss_bytes:$peak_command_rss_bytes,
      cache_bytes:0, artifact_count:0, artifact_bytes:0,
      artifact_sha256:$artifact_sha256, compilations:0, hits:0, misses:0,
      non_cacheable:0, phase_profile:{}, command:$command,
      working_directory:"/src",
      artifacts:{command_log:"phases/daemon-start.log",
        resource_usage:"phases/daemon-start-resources.txt",
        cache_report:"phases/daemon-start-cache-report.json",
        output_manifest:"phases/daemon-start-outputs.json"}}' >"${daemon_prefix}.json"

run_build_phase daemon-cold "${REPO_A}"
rm -rf "${REPO_A}/target"
run_build_phase local-hit "${REPO_A}"
run_build_phase sibling-hit "${REPO_B}"
run_build_phase target-noop "${REPO_B}" true

measure::stop_rss_poller
kill "${EXTENDED_RSS_PID}" 2>/dev/null || true
wait "${EXTENDED_RSS_PID}" 2>/dev/null || true
trap - EXIT
peak_daemon_rss="$(measure::peak_daemon_rss_bytes "${RSS_CSV}")"
peak_compile_rss="$(measure::peak_compile_rss_bytes "${RSS_CSV}")"
extended_daemon_rss="$(awk -F, '$5 == "soldr-daemon" && $3 > max {max=$3} END {print (max + 0) * 1024}' "${RSS_CSV}")"
extended_compile_rss="$(awk -F, '$5 ~ /^(clang|clang-[0-9]+|clang\+\+|emcc|em\+\+)$/ && $3 > max {max=$3} END {print (max + 0) * 1024}' "${RSS_CSV}")"
(( extended_daemon_rss > peak_daemon_rss )) && peak_daemon_rss="${extended_daemon_rss}"
(( extended_compile_rss > peak_compile_rss )) && peak_compile_rss="${extended_compile_rss}"
timed_compile_rss="$({
    jq -s 'map(.peak_command_rss_bytes) | max' \
        "${PHASES_DIR}/daemon-cold.json" \
        "${PHASES_DIR}/local-hit.json" \
        "${PHASES_DIR}/sibling-hit.json" \
        "${PHASES_DIR}/target-noop.json"
})"
(( timed_compile_rss > peak_compile_rss )) && peak_compile_rss="${timed_compile_rss}"
embedded_zccache_observed="$({
    jq -s 'any(.[]; ((.hits // 0) + (.misses // 0)) > 0)' \
        "${PHASES_DIR}/daemon-cold.json" \
        "${PHASES_DIR}/local-hit.json" \
        "${PHASES_DIR}/sibling-hit.json"
})"

jq -n \
    --arg language "${EMBEDDED_LANGUAGE}" \
    --arg fixture_sha256 "${FIXTURE_SHA256}" --arg soldr_sha "${SOLDR_SHA}" \
    --arg zccache_sha "${ZCCACHE_SHA}" --arg image_digest "${IMAGE_DIGEST}" \
    --arg host_fingerprint "${HOST_FINGERPRINT}" \
    --arg compiler_command "$(<"${RESULTS}/compiler-command.txt")" \
    --argjson embedded_zccache_observed "${embedded_zccache_observed}" \
    --argjson tool_versions "${EMBEDDED_TOOL_VERSIONS}" \
    --argjson peak_daemon_rss_bytes "${peak_daemon_rss}" \
    --argjson peak_compile_rss_bytes "${peak_compile_rss}" \
    --argjson infrastructure_valid "${_MEASURE_INFRASTRUCTURE_VALID}" \
    --argjson invalid_reasons "${_MEASURE_INVALID_REASONS_JSON}" \
    --argjson soldr_abort_count "${_MEASURE_SOLDR_ABORT_COUNT}" \
    --argjson soldr_timeout_count "${_MEASURE_SOLDR_TIMEOUT_COUNT}" \
    --argjson soldr_no_cache_retry_count "${_MEASURE_SOLDR_NO_CACHE_RETRY_COUNT}" \
    --argjson soldr_daemon_fallback_count "${_MEASURE_SOLDR_DAEMON_FALLBACK_COUNT}" \
    --argjson soldr_abort_evidence "${_MEASURE_ABORT_EVIDENCE_JSON}" \
    --argjson soldr_daemon_fallback_evidence "${_MEASURE_DAEMON_FALLBACK_EVIDENCE_JSON}" \
    --slurpfile daemon_start "${PHASES_DIR}/daemon-start.json" \
    --slurpfile daemon_cold "${PHASES_DIR}/daemon-cold.json" \
    --slurpfile local_hit "${PHASES_DIR}/local-hit.json" \
    --slurpfile sibling_hit "${PHASES_DIR}/sibling-hit.json" \
    --slurpfile target_noop "${PHASES_DIR}/target-noop.json" \
    '{schema_version:1, language:$language, fixture_sha256:$fixture_sha256,
      soldr_sha:$soldr_sha, zccache_sha:$zccache_sha, image_digest:$image_digest,
      host_fingerprint:$host_fingerprint, compiler_command:$compiler_command,
      embedded_zccache_observed:$embedded_zccache_observed,
      tool_versions:$tool_versions, peak_daemon_rss_bytes:$peak_daemon_rss_bytes,
      peak_compile_rss_bytes:$peak_compile_rss_bytes,
      infrastructure_valid:$infrastructure_valid, invalid_reasons:$invalid_reasons,
      soldr_abort_count:$soldr_abort_count, soldr_timeout_count:$soldr_timeout_count,
      soldr_no_cache_retry_count:$soldr_no_cache_retry_count,
      soldr_daemon_fallback_count:$soldr_daemon_fallback_count,
      soldr_abort_evidence:$soldr_abort_evidence,
      soldr_daemon_fallback_evidence:$soldr_daemon_fallback_evidence,
      phases:{"daemon-start":$daemon_start[0], "daemon-cold":$daemon_cold[0],
        "local-hit":$local_hit[0], "sibling-hit":$sibling_hit[0],
        "target-noop":$target_noop[0]}}' >"${RESULTS}/result.json"

SOLDR_CACHE_DIR="${CACHE}" soldr cache shutdown \
    --shutdown-timeout-seconds 30 --json >"${RESULTS}/shutdown.json" || true
measure::fail_if_infrastructure_invalid

#!/usr/bin/env bash
# Runs inside the zccache-perf-runner container. Reproduces the per-cell
# local performance matrix cell end-to-end on a
# pre-built soldr binary with this checkout's zccache embedded.
#
# Env contract (set by ci/perf_local.py):
#   SCENARIO  - cold-tar-untar-warm | worktree-share | touch-no-change
#   FIXTURE   - medium | sqlite-link
#
# Mount contract (all required, fail loud if missing):
#   /usr/local/bin/soldr    - the soldr binary (mode +x)
#   /zccache-src/           - the zccache repo (read-only)
#   /results/               - host-writable; result.json + reports land here

set -euo pipefail

require_env() {
    local var="$1"
    if [[ -z "${!var:-}" ]]; then
        echo "ERROR: env var ${var} is required" >&2
        exit 2
    fi
}

require_mount() {
    local path="$1" kind="$2"
    if [[ ! -e "${path}" ]]; then
        echo "ERROR: required ${kind} not mounted at ${path}" >&2
        exit 2
    fi
}

copy_if_exists() {
    local src="$1"
    if [[ -e "${src}" ]]; then
        cp -R "${src}" "${results_dir}/" || collection_status=$?
    fi
}

collect_scenario_evidence() {
    local scenario_root="$1" results_dir="$2" collection_status=0
    local daemon_log relative destination daemon_dir evidence
    copy_if_exists "${scenario_root}/cold-cache-report.json"
    copy_if_exists "${scenario_root}/warm-cache-report.json"
    copy_if_exists "${scenario_root}/a-cache-report.json"
    copy_if_exists "${scenario_root}/b-cache-report.json"
    copy_if_exists "${scenario_root}/cold-shutdown.json"
    copy_if_exists "${scenario_root}/warm-shutdown.json"
    copy_if_exists "${scenario_root}/worktree-shutdown.json"
    copy_if_exists "${scenario_root}/save-report.json"
    copy_if_exists "${scenario_root}/load-report.json"
    # Retain daemon startup evidence from every cache root. Startup failures are
    # intermittent and may affect the cold, warm, A, or B side independently.
    while IFS= read -r -d '' daemon_log; do
        relative="${daemon_log#"${scenario_root}/"}"
        destination="${results_dir}/daemon-runtime/${relative}"
        mkdir -p "$(dirname -- "${destination}")" &&
            cp "${daemon_log}" "${destination}" || collection_status=$?
    done < <(find "${scenario_root}" -type f -name daemon-spawn.log -print0)
    find "${scenario_root}" -type d -path '*/cache/soldr-daemon' -print0 \
        | while IFS= read -r -d '' daemon_dir; do
            find "${daemon_dir}" -maxdepth 3 -printf '%y %p -> %l\n'
        done >"${scenario_root}/daemon-files.txt" 2>/dev/null || true
    copy_if_exists "${scenario_root}/daemon-files.txt"
    copy_if_exists "${scenario_root}/cold-zccache-logs"
    copy_if_exists "${scenario_root}/warm-zccache-logs"
    copy_if_exists "${scenario_root}/rss-${SCENARIO}.csv"
    for evidence in "${scenario_root}"/soldr-aborts-*.jsonl; do
        copy_if_exists "${evidence}"
    done
    for evidence in "${scenario_root}"/soldr-daemon-fallbacks-*.jsonl; do
        copy_if_exists "${evidence}"
    done
    return "$collection_status"
}

# Collect reports even when the scenario fails; its original status takes
# precedence over collection/JSON errors. Successful scenarios still require
# successful collection and the documented single-object result contract.
run_scenario_and_collect() {
    local script="$1" fixture_dir="$2" scenario_root="$3" results_dir="$4"
    local scenario_status=0 collection_status=0 validation_status=0
    local -a pipeline_status=(0 0)
    bash "$script" "$fixture_dir" 2>"${results_dir}/scenario-stderr.log" \
        | tee "${results_dir}/scenario-stdout.log" || pipeline_status=("${PIPESTATUS[@]}")
    scenario_status="${pipeline_status[0]}"
    collection_status="${pipeline_status[1]}"
    cat "${results_dir}/scenario-stderr.log" >&2 || collection_status=$?
    collect_scenario_evidence "$scenario_root" "$results_dir" || collection_status=$?
    # soldr can print JSON before the scenario's final result object.
    tail -n 1 "${results_dir}/scenario-stdout.log" >"${results_dir}/result.json" \
        && jq -e 'type == "object"' "${results_dir}/result.json" >/dev/null \
        || validation_status=$?
    if (( scenario_status != 0 )); then
        return "$scenario_status"
    fi
    if (( collection_status != 0 )); then
        return "$collection_status"
    fi
    return "$validation_status"
}

main() {
    require_env SCENARIO
    require_env FIXTURE
    require_mount /usr/local/bin/soldr file
    require_mount /zccache-src dir
    require_mount /results dir

    # The soldr binary is mounted read-only from the host's binaries/ dir.
    # The builder image set +x on it at build time so no chmod is needed
    # here (and chmod would fail on a read-only mount).
    # Work dir for the fixture extraction. The scenario scripts write under
    # this dir and expect to own it. We use /tmp/perf-work so the persistent
    # /results volume isn't polluted with intermediate state.
    WORK_DIR="/tmp/perf-work-${SCENARIO}"
    rm -rf "${WORK_DIR}"
    mkdir -p "${WORK_DIR}"

    # Step 1: extract the fixture tarball into ${WORK_DIR}/${FIXTURE}/
    bash "/zccache-src/perf/lib/extract.sh" "${FIXTURE}" "${WORK_DIR}"

    # Step 2: run the scenario. It owns its own cache-cold/ + cache-warm/
    # subdirs under the parent of the fixture (i.e. ${WORK_DIR}/).
    scenario_script="/zccache-src/perf/scenarios/${SCENARIO}/run.sh"
    if [[ ! -f "${scenario_script}" ]]; then
        echo "ERROR: scenario script not found: ${scenario_script}" >&2
        exit 2
    fi
    run_scenario_and_collect "$scenario_script" "$WORK_DIR/$FIXTURE" "$WORK_DIR" /results
    echo "DONE. Results in /results/:"
    ls -la /results/
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    main "$@"
fi

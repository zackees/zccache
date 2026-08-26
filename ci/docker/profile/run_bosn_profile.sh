#!/usr/bin/env bash

# Run the existing perf_bench_test binary under complementary Linux profilers.
# Bosn owns this script's container and persistent build/toolchain volumes.
set -euo pipefail

MODE=${1:-all}
WORKLOAD=${ZCCACHE_PROFILE_WORKLOAD:-perf_rustc_zccache_vs_sccache}
SAMPLE_HZ=${ZCCACHE_PROFILE_HZ:-99}
SOLDR_SENTINEL="/bosn/rustup/.standalone-toolchain-${SOLDR_VERSION}-${RUST_VERSION}"

seed_soldr_home() {
    if [[ ! -f "/opt/soldr-seed/.standalone-toolchain-${SOLDR_VERSION}-${RUST_VERSION}" ]]; then
        echo "ERROR: cached soldr toolchain seed is incomplete" >&2
        exit 2
    fi
    if [[ ! -f "${SOLDR_SENTINEL}" ]]; then
        mkdir -p /bosn/cargo /bosn/rustup
        cp -a /opt/soldr-seed/.soldr/cargo/. /bosn/cargo/
        cp -a /opt/soldr-seed/.soldr/rustup/. /bosn/rustup/
        touch "${SOLDR_SENTINEL}"
    fi
    export CARGO_HOME=/bosn/cargo
    export RUSTUP_HOME=/bosn/rustup
    export PATH="${CARGO_HOME}/bin:${PATH}"
    if ! command -v rustc >/dev/null 2>&1; then
        echo "ERROR: soldr's seeded rustc proxy is not on PATH" >&2
        exit 2
    fi
}

find_benchmark() {
    find /target/release/deps -maxdepth 1 -type f \
        -name 'perf_bench_test-*' -perm /111 -printf '%T@ %p\n' \
        | sort -nr | head -n 1 | cut -d' ' -f2-
}

build_benchmark() {
    seed_soldr_home
    cd /work
    # The Windows checkout may contain repo-local .cargo/.rustup homes. This
    # container intentionally injects its seeded Linux homes, so retain them
    # instead of re-resolving the bind-mounted host workspace.
    soldr --trust-inherited-soldr-env cargo test \
        -p zccache --test perf_bench_test --release --no-run
    TEST_BIN=$(find_benchmark)
    if [[ -z "${TEST_BIN}" || ! -x "${TEST_BIN}" ]]; then
        echo "ERROR: perf_bench_test executable was not produced" >&2
        exit 2
    fi
}

prepare_run() {
    build_benchmark
    local revision
    revision=$(git -C /work rev-parse --short=12 HEAD)
    RUN_ID=$(date -u +%Y%m%dT%H%M%SZ)
    OUT_DIR="/work/.perf-local/bosn-profile/${revision}/${WORKLOAD}/${RUN_ID}"
    mkdir -p "${OUT_DIR}"
    {
        echo "revision=$(git -C /work rev-parse HEAD)"
        # The Windows checkout contains a small CRLF subset that Linux Git
        # sees as rewritten. Ignore only end-of-line whitespace so provenance
        # still fails closed on semantic staged or unstaged content changes.
        echo "dirty=$(if git -C /work -c core.filemode=false diff --ignore-space-at-eol --quiet HEAD --; then echo false; else echo true; fi)"
        echo "workload=${WORKLOAD}"
        echo "kernel=$(uname -r)"
        echo "soldr=$(soldr version | head -n 1)"
        echo "rustc=$(soldr rustc --version | head -n 1)"
        echo "clang=$(clang++ --version | head -n 1)"
        echo "sccache=$(sccache --version | head -n 1)"
        echo "emscripten=$(em++ --version | head -n 1)"
        echo "benchmark=${TEST_BIN}"
        sha256sum "${TEST_BIN}"
    } > "${OUT_DIR}/provenance.txt"
    echo "evidence: ${OUT_DIR}"
}

run_bench() {
    prepare_run
    /usr/bin/time -v -o "${OUT_DIR}/resource-usage.txt" \
        "${TEST_BIN}" "${WORKLOAD}" --nocapture --ignored --test-threads=1 \
        > "${OUT_DIR}/benchmark.stdout.log" \
        2> "${OUT_DIR}/benchmark.stderr.log"
}

run_oncpu() {
    prepare_run
    local perf_exit=127
    if command -v perf >/dev/null 2>&1; then
        set +e
        perf record -F "${SAMPLE_HZ}" -g --call-graph fp \
            -o "${OUT_DIR}/perf.data" -- \
            "${TEST_BIN}" "${WORKLOAD}" --nocapture --ignored --test-threads=1 \
            > "${OUT_DIR}/oncpu.stdout.log" \
            2> "${OUT_DIR}/oncpu.stderr.log"
        perf_exit=$?
        set -e
    else
        echo "perf is not installed for this pinned base image" \
            > "${OUT_DIR}/oncpu.stderr.log"
    fi
    echo "${perf_exit}" > "${OUT_DIR}/perf.exit"
    if [[ ${perf_exit} -eq 0 && -s "${OUT_DIR}/perf.data" ]]; then
        perf report --stdio --no-children --percent-limit 0.25 \
            -i "${OUT_DIR}/perf.data" > "${OUT_DIR}/perf-report.txt"
        perf script -i "${OUT_DIR}/perf.data" > "${OUT_DIR}/perf-script.txt"
        return
    fi

    echo "perf unavailable (exit ${perf_exit}); using Callgrind" >&2
    valgrind --tool=callgrind --trace-children=no \
        --callgrind-out-file="${OUT_DIR}/callgrind.out" \
        "${TEST_BIN}" "${WORKLOAD}" --nocapture --ignored --test-threads=1 \
        > "${OUT_DIR}/callgrind.stdout.log" \
        2> "${OUT_DIR}/callgrind.stderr.log"
    callgrind_annotate --auto=yes --inclusive=yes \
        "${OUT_DIR}/callgrind.out" > "${OUT_DIR}/callgrind-report.txt"
}

run_offcpu() {
    prepare_run
    /usr/bin/time -v -o "${OUT_DIR}/offcpu-resource-usage.txt" \
        strace -f -w -c -o "${OUT_DIR}/strace-wall-summary.txt" \
        "${TEST_BIN}" "${WORKLOAD}" --nocapture --ignored --test-threads=1 \
        > "${OUT_DIR}/offcpu.stdout.log" \
        2> "${OUT_DIR}/offcpu.stderr.log"
}

run_heaptrack() {
    prepare_run
    /usr/bin/time -v -o "${OUT_DIR}/heaptrack-resource-usage.txt" \
        heaptrack -o "${OUT_DIR}/heaptrack" \
        "${TEST_BIN}" "${WORKLOAD}" --nocapture --ignored --test-threads=1 \
        > "${OUT_DIR}/heaptrack.stdout.log" \
        2> "${OUT_DIR}/heaptrack.stderr.log"
    local heaptrack_file
    heaptrack_file=$(find "${OUT_DIR}" -maxdepth 1 -type f \
        \( -name 'heaptrack*.gz' -o -name 'heaptrack*.zst' \) -print -quit)
    if [[ -n "${heaptrack_file}" ]]; then
        heaptrack_print "${heaptrack_file}" > "${OUT_DIR}/heaptrack-report.txt"
    fi
}

run_massif() {
    prepare_run
    valgrind --tool=massif --stacks=yes --pages-as-heap=yes --time-unit=ms \
        --massif-out-file="${OUT_DIR}/massif.out" \
        "${TEST_BIN}" "${WORKLOAD}" --nocapture --ignored --test-threads=1 \
        > "${OUT_DIR}/massif.stdout.log" \
        2> "${OUT_DIR}/massif.stderr.log"
    ms_print "${OUT_DIR}/massif.out" > "${OUT_DIR}/massif-report.txt"
}

run_memory() {
    run_heaptrack
    run_massif
}

case "${MODE}" in
    build)
        build_benchmark
        ;;
    bench)
        run_bench
        ;;
    oncpu)
        run_oncpu
        ;;
    offcpu)
        run_offcpu
        ;;
    memory)
        run_memory
        ;;
    heaptrack)
        run_heaptrack
        ;;
    massif)
        run_massif
        ;;
    all)
        run_bench
        run_oncpu
        run_offcpu
        run_memory
        ;;
    *)
        echo "usage: run_bosn_profile.sh {build|bench|oncpu|offcpu|heaptrack|massif|memory|all}" >&2
        exit 2
        ;;
esac

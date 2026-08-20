#!/usr/bin/env bash

set -euo pipefail

require_mount() {
    local path="$1"
    if [[ ! -e "${path}" ]]; then
        echo "ERROR: required mount missing: ${path}" >&2
        exit 2
    fi
}

require_mount /src/Cargo.toml
require_mount /artifacts
require_mount /results

seed_soldr_home() {
    local soldr_root="${HOME}/.soldr"
    local sentinel=".standalone-toolchain-${SOLDR_VERSION}-${RUST_VERSION}"
    if [[ ! -f "/opt/soldr-seed/${sentinel}" ]]; then
        echo "ERROR: cached soldr toolchain seed is incomplete" >&2
        exit 2
    fi
    if [[ ! -f "${soldr_root}/${sentinel}" ]]; then
        mkdir -p "${soldr_root}"
        find "${soldr_root}" -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +
        cp -a /opt/soldr-seed/.soldr/. "${soldr_root}/"
        touch "${soldr_root}/${sentinel}"
    fi
    export CARGO_HOME="${soldr_root}/cargo"
    export RUSTUP_HOME="${soldr_root}/rustup"
}

command="${1:-}"
case "${command}" in
    build)
        seed_soldr_home
        cd /src
        soldr cargo test -p zccache --test perf_bench_test --release --no-run
        soldr cargo build -p zccache --features ci-bin --bin zccache-ci --release
        benchmark="$({
            find /target/release/deps -maxdepth 1 -type f \
                -name 'perf_bench_test-*' -perm /111 -printf '%T@ %p\n'
        } | sort -nr | head -n 1 | cut -d' ' -f2-)"
        if [[ -z "${benchmark}" || ! -x "${benchmark}" ]]; then
            echo "ERROR: perf_bench_test executable was not produced" >&2
            exit 2
        fi
        install -m 0755 "${benchmark}" /artifacts/perf_bench_test
        install -m 0755 /target/release/zccache-ci /artifacts/zccache-ci
        ;;
    verify)
        seed_soldr_home
        benchmark_sha256=""
        zccache_ci_sha256=""
        if [[ -f /artifacts/perf_bench_test ]]; then
            benchmark_sha256="$(sha256sum /artifacts/perf_bench_test | cut -d' ' -f1)"
        fi
        if [[ -f /artifacts/zccache-ci ]]; then
            zccache_ci_sha256="$(sha256sum /artifacts/zccache-ci | cut -d' ' -f1)"
        fi
        jq -n \
            --arg rustc "$(soldr rustc --version | head -n 1)" \
            --arg clang "$(clang++ --version | head -n 1)" \
            --arg sccache "$(sccache --version | head -n 1)" \
            --arg emscripten "$(em++ --version | head -n 1)" \
            --arg soldr "$(soldr version | head -n 1)" \
            --arg benchmark_sha256 "${benchmark_sha256}" \
            --arg zccache_ci_sha256 "${zccache_ci_sha256}" \
            '{rustc: $rustc, clang: $clang, sccache: $sccache, emscripten: $emscripten, soldr: $soldr, benchmark_sha256: $benchmark_sha256, zccache_ci_sha256: $zccache_ci_sha256}'
        ;;
    run)
        require_mount /artifacts/perf_bench_test
        require_mount /artifacts/zccache-ci
        language="${2:?language is required}"
        test_name="${3:?test name is required}"
        attempts="${4:?attempt count is required}"
        export ZCCACHE_CI_BIN=/artifacts/zccache-ci
        cd /src
        /usr/bin/time -v -o /results/resource-usage.txt \
            uv run --no-project python -m ci.perf_guard \
                --run-benchmarks \
                --language "${language}" \
                --test "${test_name}" \
                --attempts "${attempts}" \
                --collect-all-attempts \
                --benchmark-binary /artifacts/perf_bench_test \
                --output-dir /results
        ;;
    *)
        echo "usage: standalone-perf {build|verify|run <language> <test> <attempts>}" >&2
        exit 2
        ;;
esac

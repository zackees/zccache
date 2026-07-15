#!/usr/bin/env bash

set -euo pipefail

require_env() {
    local name="$1"
    if [[ -z "${!name:-}" ]]; then
        echo "ERROR: ${name} is required" >&2
        exit 2
    fi
}

require_path() {
    local path="$1"
    if [[ ! -e "${path}" ]]; then
        echo "ERROR: required path is missing: ${path}" >&2
        exit 2
    fi
}

seed_soldr_home() {
    local soldr_root="${HOME}/.soldr"
    local seed_sentinel="/opt/soldr-seed/.standalone-toolchain-${SOLDR_VERSION}-${RUST_VERSION}"
    local sentinel=".embedded-toolchain-${IMAGE_DIGEST#sha256:}"
    require_path "${seed_sentinel}"
    if [[ ! -f "${soldr_root}/${sentinel}" ]]; then
        mkdir -p "${soldr_root}"
        find "${soldr_root}" -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +
        cp -a /opt/soldr-seed/.soldr/. "${soldr_root}/"
        touch "${soldr_root}/${sentinel}"
    fi
    export CARGO_HOME="${soldr_root}/cargo"
    export RUSTUP_HOME="${soldr_root}/rustup"
}

for name in \
    EMBEDDED_LANGUAGE FIXTURE_SHA256 SOLDR_SHA ZCCACHE_SHA \
    IMAGE_DIGEST HOST_FINGERPRINT; do
    require_env "${name}"
done
require_path /usr/local/bin/soldr
require_path /zccache-src/perf/fixtures/embedded-mixed/Cargo.toml
require_path /results

seed_soldr_home

work_root="/tmp/embedded-${EMBEDDED_LANGUAGE}"
rm -rf "${work_root}"
mkdir -p "${work_root}/repo-a"
cp -a /zccache-src/perf/fixtures/embedded-mixed/. "${work_root}/repo-a/"

export EMBEDDED_WORK_ROOT="${work_root}"
export EMBEDDED_FIXTURE="${work_root}/repo-a"
export EMBEDDED_TOOL_VERSIONS="$({
    jq -cn \
        --arg soldr "$(soldr version | head -n 1)" \
        --arg rustc "$(soldr rustc --version | head -n 1)" \
        --arg clang "$(clang++ --version | head -n 1)" \
        --arg emscripten "$(em++ --version | head -n 1)" \
        '{soldr: $soldr, rustc: $rustc, clang: $clang, emscripten: $emscripten}'
})"

bash /zccache-src/perf/scenarios/embedded-lifecycle/run.sh

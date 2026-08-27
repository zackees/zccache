#!/usr/bin/env bash

# Run repository lint and unit-test contracts in Bosn's Linux toolchain.
# The checkout is bind-mounted from the host and may contain Windows-local
# .cargo/.rustup homes, so every Rust command runs outside it with the seeded
# Linux homes explicitly preserved.
set -euo pipefail

MODE="${1:-}"
case "${MODE}" in
    lint|test)
        ;;
    *)
        echo "usage: run_bosn_check.sh {lint|test}" >&2
        exit 2
        ;;
esac

SOLDR_SENTINEL="/zccache-profile/rustup/.standalone-toolchain-${SOLDR_VERSION}-${RUST_VERSION}"
PREBUILD_BIN_FEATURES="zccache-bin,daemon-bin,download-bin,download-daemon-bin,fingerprint-bin,stamp-bin,ci-bin,crash-tools,tokio-console,test-support"

seed_soldr_home() {
    if [[ ! -f "/opt/soldr-seed/.standalone-toolchain-${SOLDR_VERSION}-${RUST_VERSION}" ]]; then
        echo "ERROR: cached soldr toolchain seed is incomplete" >&2
        exit 2
    fi
    if [[ ! -f "${SOLDR_SENTINEL}" ]]; then
        mkdir -p /zccache-profile/cargo /zccache-profile/rustup
        cp -a /opt/soldr-seed/.soldr/cargo/. /zccache-profile/cargo/
        cp -a /opt/soldr-seed/.soldr/rustup/. /zccache-profile/rustup/
        touch "${SOLDR_SENTINEL}"
    fi
    export CARGO_HOME=/zccache-profile/cargo
    export RUSTUP_HOME=/zccache-profile/rustup
    export PATH="${CARGO_HOME}/bin:${PATH}"
    if ! command -v cargo >/dev/null 2>&1; then
        echo "ERROR: soldr's seeded cargo proxy is not on PATH" >&2
        exit 2
    fi
}

run_soldr() {
    cd /tmp
    soldr --trust-inherited-soldr-env "$@"
}

seed_soldr_home
case "${MODE}" in
    lint)
        run_soldr cargo fmt --manifest-path /work/Cargo.toml --all -- --check
        run_soldr cargo clippy --manifest-path /work/Cargo.toml --workspace --all-targets -- -D warnings
        ;;
    test)
        # Mirror ci/test.py's required-bin prebuild and serial unit-test path
        # without letting its /work subprocess rediscover host toolchain homes.
        run_soldr cargo build --manifest-path /work/Cargo.toml -p zccache --bins --features "${PREBUILD_BIN_FEATURES}"
        run_soldr cargo test --manifest-path /work/Cargo.toml --workspace -- --test-threads=1
        ;;
esac

#!/usr/bin/env bash
set -euo pipefail
features="zccache/zccache-bin,zccache/daemon-bin,zccache/download-bin,zccache/download-daemon-bin,zccache/fingerprint-bin,zccache/stamp-bin,zccache/ci-bin,zccache/crash-tools,zccache/tokio-console,zccache/test-support"
cargo test --workspace --features "$features" --no-fail-fast --no-run
cargo test --workspace --features "$features" --no-fail-fast -- --test-threads=1

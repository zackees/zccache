#!/usr/bin/env bash
# The local gate's `tests` lane, inside the isolated bosn container
# (bosn.toml [task.gate-test]; zackees/ci.yml#168 GATE-005).
#
# Runs exactly the test commands of the remote Integration (Linux) job
# (.github/workflows/integration.yml) plus the MSRV job's nested Dylint
# cache contract (.github/workflows/ci.yml), so an attested head may skip
# them (local-gate.toml [gate.trust], ci-attestations.yml). Keep the two in
# step: a command added there and not here is a command the gate never ran.
set -euo pipefail

cd "$(dirname "$0")/../../.."

# zackees/ci.yml#196 (GATE-009): prove this container sees the worktree the
# gate is attesting. The gate wrote a fresh nonce to .gate-nonce and fails
# the lane unless this line echoes it back.
echo "gate-nonce: $(cat .gate-nonce 2>/dev/null || echo missing)"

export CARGO_TARGET_DIR=/target
export CARGO_TERM_COLOR=always
# integration.yml / ci.yml workflow env.
export RUSTFLAGS="-D warnings"
FEATURES="zccache/zccache-bin,zccache/daemon-bin,zccache/download-bin,zccache/download-daemon-bin,zccache/fingerprint-bin,zccache/stamp-bin,zccache/ci-bin,zccache/crash-tools,zccache/tokio-console,zccache/test-support,zccache/heap-profile"

step() { echo "::: $*"; }

# integration.yml "Build integration test binaries".
step "build integration test binaries"
soldr cargo test --workspace --features "$FEATURES" --no-fail-fast --no-run
soldr cargo build -p zccache --features ci-bin --bin zccache-ci

# integration.yml "Test (full workspace)": nextest, then the doctests.
step "test (full workspace, nextest)"
soldr cargo nextest run --workspace --features "$FEATURES" --no-fail-fast
step "doctests"
soldr cargo test --workspace --features "$FEATURES" --no-fail-fast --doc

# integration.yml "Wrapper daemon-unavailable contract (exit 125)".
step "wrapper daemon-unavailable contract"
soldr cargo test -p zccache --features "$FEATURES" --test cli cli_wrapper_failure_boundaries:: -- --ignored --test-threads=1

# integration.yml "Strict artifact-layout validation".
step "strict artifact-layout validation"
ZCCACHE_DISABLE=1 soldr cargo test -p zccache-daemon-core --test legacy_path_validation -- --ignored --test-threads=1

# ci.yml msrv "Verify nested Dylint cache contract".
step "nested Dylint cache contract"
soldr cargo test -p zccache --test cache_rust daemon_dylint_cache_test:: -- --ignored

step "gate tests passed"

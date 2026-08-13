"""Lockstep tests for zccache's own Rust toolchain declarations (zccache#1365).

The workspace MSRV is a single number declared in many places. This module
keeps them in lockstep: every file that builds, lints, or documents the
toolchain used to compile zccache itself must pin the current MSRV, and the
legacy version may remain only in fixtures that intentionally model another
compiler.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

EXPECTED = "1.95.0"
LEGACY = "1.94.1"

# Files that build, lint, or document zccache's own toolchain. Each must
# declare EXPECTED and must not mention LEGACY.
MUST_PIN: tuple[str, ...] = (
    "Cargo.toml",  # workspace rust-version
    "rust-toolchain.toml",
    ".clippy.toml",
    "CLAUDE.md",
    "docs/ROADMAP.md",
    ".github/actions/build-target/action.yml",
    ".github/workflows/bench-action.yml",
    ".github/workflows/bench-fingerprint.yml",
    ".github/workflows/broker-stress.yml",
    ".github/workflows/ci-check-cross.yml",
    ".github/workflows/ci-check.yml",
    ".github/workflows/ci.yml",
    ".github/workflows/clippy.yml",
    ".github/workflows/coverage.yml",
    ".github/workflows/fs-matrix.yml",
    ".github/workflows/integration.yml",
    ".github/workflows/release-auto.yml",
    ".github/workflows/test-action.yml",
    "ci/docker/zccache-builder.Dockerfile",
    "ci/docker/soldr-builder.Dockerfile",
    "ci/docker/runner.Dockerfile",
    "ci/docker/standalone-perf.Dockerfile",
    "ci/docker/profile/Dockerfile.perf-linux",
    "ci/docker/profile/README.md",
    "Dockerfile.cc-test",
    "Dockerfile.jobserver-test",
)

# Files that intentionally keep LEGACY: fixture toolchains modeling a user's
# compiler, audit/plan fixtures with arbitrary compiler identity strings,
# historical docs, and this checker itself (which spells LEGACY out).
INTENTIONAL_LEGACY: tuple[str, ...] = (
    "ci/tests/test_toolchain_consistency.py",
    "perf/fixtures/medium/rust-toolchain.toml",
    "perf/fixtures/sqlite-link/rust-toolchain.toml",
    "crates/zccache/tests/audit-fixtures/embedded-cold-compile.jsonl",
    "crates/zccache/tests/audit-fixtures/embedded-cancelled-compile.jsonl",
    "crates/zccache-artifact/src/rust_plan/tests/mod.rs",
    "crates/zccache-depgraph/src/context/tests/rustc.rs",
    "docs/architecture/rust-artifact-plan.md",
    "ci/perf_standalone.py",
    "ci/tests/test_perf_standalone.py",
    "ci/tests/test_perf_embedded.py",
    "ci/tests/test_host_diag.py",
)


def _read(relative: str) -> str:
    return (ROOT / relative).read_text(encoding="utf-8", errors="replace")


def _tracked_files_with_legacy() -> set[str]:
    result = subprocess.run(
        ["git", "grep", "-l", "-I", LEGACY],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    files = {line.replace("\\", "/") for line in result.stdout.splitlines()}
    # Lockfile and build artifacts are not declarations.
    files.discard("Cargo.lock")
    return files


def test_zccache_own_toolchain_declarations_pin_expected() -> None:
    for relative in MUST_PIN:
        text = _read(relative)
        assert EXPECTED in text, f"{relative} must declare {EXPECTED}"
        assert LEGACY not in text, f"{relative} still declares {LEGACY}"


def test_legacy_toolchain_remains_only_in_intentional_fixtures() -> None:
    actual = _tracked_files_with_legacy()
    expected = set(INTENTIONAL_LEGACY)
    missing = expected - actual
    unexpected = actual - expected
    assert not missing, f"intentional LEGACY files no longer mention it: {sorted(missing)}"
    assert not unexpected, (
        f"unexpected files mentioning {LEGACY}: {sorted(unexpected)}"
    )

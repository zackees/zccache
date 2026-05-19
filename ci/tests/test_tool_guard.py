from __future__ import annotations

import importlib.util
from pathlib import Path


def _load_tool_guard():
    module_path = Path(__file__).resolve().parents[1] / "hooks" / "tool_guard.py"
    spec = importlib.util.spec_from_file_location("tool_guard", module_path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


tool_guard = _load_tool_guard()


def _check(command: str):
    return tool_guard.check_command(command)


# ── bare-tool blocking ──────────────────────────────────────────────────────


def test_bare_cargo_blocked():
    result = _check("cargo build")
    assert result is not None
    assert result[0] == "cargo"
    assert "soldr cargo" in result[1]


def test_bare_rustc_blocked():
    result = _check("rustc --print sysroot")
    assert result is not None
    assert result[0] == "rustc"


def test_bare_rustfmt_blocked():
    assert _check("rustfmt foo.rs") is not None


# ── rustup gating (regression guard) ────────────────────────────────────────


def test_bare_rustup_blocked():
    """Pre-existing escape hatch: `rustup run <toolchain> cargo ...` bypassed
    soldr's toolchain selection. After this fix, rustup itself must be
    prefixed with `soldr `."""
    result = _check("rustup run nightly-2026-03-26 cargo check")
    assert result is not None
    assert result[0] == "rustup"


def test_bare_rustup_which_blocked():
    """`rustup which cargo` is another way to fish out a non-soldr cargo."""
    assert _check("rustup which cargo") is not None


def test_rustup_via_soldr_passes():
    """`soldr rustup` is a documented passthrough — allowed."""
    assert _check("soldr rustup run nightly-2026-03-26 cargo check") is None


# ── cargo +toolchain escape ─────────────────────────────────────────────────


def test_cargo_plus_toolchain_blocked():
    """`cargo +nightly check` invokes the rustup shim's toolchain selector
    directly, bypassing soldr."""
    assert _check("cargo +nightly-2026-03-26 check") is not None


# ── related rust tools ──────────────────────────────────────────────────────


def test_rustdoc_blocked():
    assert _check("rustdoc --crate-name foo") is not None


def test_rust_analyzer_blocked():
    assert _check("rust-analyzer diagnostics") is not None


# ── allowed shapes ──────────────────────────────────────────────────────────


def test_soldr_cargo_passes():
    assert _check("soldr cargo check --workspace") is None


def test_unrelated_command_passes():
    assert _check("ls -la") is None
    assert _check("git status") is None


# ── env-var prefix doesn't mask the tool ────────────────────────────────────


def test_env_prefix_does_not_mask_bare_cargo():
    """`RUSTFLAGS=... cargo build` is still bare cargo and must be blocked.
    Note: the hook's tokenizer is whitespace-split, so quoted env values
    that contain spaces aren't recognized as one token. This guard uses a
    space-free value to stay within the tokenizer's contract."""
    assert _check("RUSTFLAGS=-Dwarnings cargo build") is not None


def test_env_prefix_does_not_mask_bare_rustup():
    assert _check("FOO=bar rustup run nightly cargo build") is not None


# ── uv-run wrapper ──────────────────────────────────────────────────────────


def test_uv_run_cargo_blocked():
    """`uv run cargo` bypasses soldr's toolchain selection — already
    covered, this is a regression guard."""
    assert _check("uv run cargo build") is not None


def test_uv_pip_passes():
    assert _check("uv pip install foo") is None

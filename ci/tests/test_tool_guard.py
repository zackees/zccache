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


def _extract(payload):
    return tool_guard.extract_command(payload)


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


def test_extract_command_from_shell_command_payload():
    assert (
        _extract(
            {
                "tool_name": "functions.shell_command",
                "tool_input": {"command": "cargo build"},
            }
        )
        == "cargo build"
    )


def test_extract_command_from_shell_script_payload():
    assert (
        _extract({"tool_name": "Shell", "tool_input": {"script": "cargo build"}})
        == "cargo build"
    )


def test_extract_command_from_argv_payload():
    assert (
        _extract(
            {
                "tool_name": "PowerShell",
                "tool_input": {"argv": ["cargo", "build"]},
            }
        )
        == "cargo build"
    )


# ── rustup gating (regression guard) ────────────────────────────────────────


def test_bare_rustup_blocked():
    """Pre-existing escape hatch: `rustup run <toolchain> cargo ...` bypassed
    soldr's toolchain selection. After this fix, rustup itself must be
    prefixed with `soldr `."""
    result = _check("rustup run nightly-2026-05-26 cargo check")
    assert result is not None
    assert result[0] == "rustup"


def test_bare_rustup_which_blocked():
    """`rustup which cargo` is another way to fish out a non-soldr cargo."""
    assert _check("rustup which cargo") is not None


def test_rustup_via_soldr_passes():
    """`soldr rustup` is a documented passthrough — allowed."""
    assert _check("soldr rustup run nightly-2026-05-26 cargo check") is None


# ── cargo +toolchain escape ─────────────────────────────────────────────────


def test_cargo_plus_toolchain_blocked():
    """`cargo +nightly check` invokes the rustup shim's toolchain selector
    directly, bypassing soldr."""
    assert _check("cargo +nightly-2026-05-26 check") is not None


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


# ── forbidden test/bench Python paths ──────────────────────────────────────
#
# Original purpose of the FORBIDDEN_PATH_RE check: block running Python on
# files under bench/ or tests/ since per project policy those should be
# Rust. The check is scoped to actual Python invocations (direct or via
# `uv run python`) — references to test paths as arguments to OTHER tools
# (pytest, wc, cat, gh) are allowed.


def test_bare_python_on_test_file_blocked():
    """`python tests/foo.py` runs Python on a test file → forbidden."""
    result = _check("python tests/foo.py")
    assert result is not None
    assert result[0] == "python"


def test_bare_python3_on_test_file_blocked():
    """`python3 tests/foo.py` blocks. The hook returns the generic
    `"python"` tool identifier for any Python-on-test-file regardless of
    the exact executor (`python`/`python3`/`uv run python`) so the deny
    message is uniform."""
    result = _check("python3 tests/foo.py")
    assert result is not None
    assert result[0] == "python"


def test_bare_python_on_bench_file_blocked():
    """Same rule for bench/."""
    assert _check("python bench/runner.py") is not None


def test_uv_run_python_on_test_file_blocked():
    """`uv run python tests/foo.py` is the escape hatch the original check
    was built to catch — must still block."""
    assert _check("uv run python tests/foo.py") is not None


def test_uv_run_python3_on_test_file_blocked():
    assert _check("uv run python3 tests/foo.py") is not None


def test_env_prefix_does_not_mask_python_test_block():
    """`FOO=bar python tests/foo.py` — env prefix stripped, still Python on
    a test file."""
    assert _check("FOO=bar python tests/foo.py") is not None


def test_uv_run_script_into_test_file_blocked():
    """`uv run --script tests/foo.py` — the path IS the script being run.
    Block."""
    assert _check("uv run --script tests/foo.py") is not None


def test_python_test_blocked_in_pipeline():
    """`python tests/foo.py | head -10` — first segment is Python on a
    test file, must block."""
    assert _check("python tests/foo.py | head -10") is not None


def test_python_test_blocked_after_chain():
    """`cd tmp && python tests/foo.py` — Python invocation in the second
    `&&`-segment, must block."""
    assert _check("cd tmp && python tests/foo.py") is not None


# ── path-as-arg to non-Python tools: should NOT block ──────────────────────
#
# These were all false-positive-blocked by the previous regex (which
# matched test-path substrings anywhere in the command). The tightened
# regex now only fires on Python invocations.


def test_pytest_against_tests_dir_passes():
    """`pytest ci/tests/test_x.py` — pytest is the runner, must pass."""
    assert _check("pytest ci/tests/test_tool_guard.py") is None


def test_uv_run_pytest_against_tests_dir_passes():
    """`uv run pytest ci/tests/...` — the documented way to run the
    repo's CI test suite (see ci/tests/README.md)."""
    assert _check("uv run pytest ci/tests/test_tool_guard.py") is None


def test_wc_on_test_file_passes():
    """`wc -l tests/foo.py` — counts lines, doesn't execute. Allowed."""
    assert _check("wc -l tests/foo.py") is None


def test_cat_on_test_file_passes():
    """`cat ci/tests/test_x.py` — dumps content, doesn't execute."""
    assert _check("cat ci/tests/test_tool_guard.py") is None


def test_gh_api_with_test_path_in_url_passes():
    """`gh api .../tests/foo.py` — the test path is part of an API URL,
    not an executable."""
    assert _check("gh api repos/o/r/contents/tests/test_x.py") is None


def test_grep_in_test_dir_passes():
    """`grep -r foo ci/tests/` — read-only file search."""
    assert _check("grep -r foo ci/tests/") is None


def test_find_test_files_passes():
    """`find ci/tests -name 'test_*.py'` — directory traversal."""
    assert _check("find ci/tests -name 'test_*.py'") is None


def test_echo_mentioning_test_path_passes():
    """`echo 'see tests/foo.py'` — text mention, not an invocation."""
    assert _check("echo 'see tests/foo.py for example'") is None


def test_fetch_then_grep_pipeline_passes():
    """`gh api ... | grep ...` — chained read-only, no Python anywhere."""
    assert _check("gh api repos/o/r/contents/tests/x.py | grep TODO") is None


def test_semicolon_separated_non_python_passes():
    """`wc -l tests/x.py ; cat tests/y.py` — two non-Python segments."""
    assert _check("wc -l tests/x.py ; cat tests/y.py") is None


# ── boundary cases for the path regex ──────────────────────────────────────


def test_python_on_non_test_path_passes_path_check_then_blocks_bare():
    """`python foo.py` — no forbidden path, but bare python is still blocked
    for the generic "use uv run" reason."""
    result = _check("python foo.py")
    assert result is not None
    # Either error is acceptable; we're just verifying the test-path check
    # didn't fire (because foo.py isn't under tests/ or bench/).


def test_python_on_path_like_test_but_not_dir_passes_test_check():
    """`uv run python tests-fixture/foo.py` — `tests-fixture` isn't `tests`;
    the regex requires the exact dir name. Should NOT trigger test-path
    block (uv run python on non-test paths is allowed)."""
    assert _check("uv run python tests-fixture/foo.py") is None


def test_python_on_path_with_tests_segment_blocked():
    """`uv run python foo/tests/bar.py` — `tests/` is a path segment
    (preceded by `/`), regex fires correctly."""
    assert _check("uv run python foo/tests/bar.py") is not None


def test_python_with_no_test_path_in_args_blocks_for_bare_python():
    """`python --version` — bare python, blocks for the generic reason
    (not the test-path reason)."""
    result = _check("python --version")
    assert result is not None
    # Bare python message starts with "Use `uv run ...`"
    assert "uv run" in result[1]


def test_uv_run_python_with_no_test_path_passes():
    """`uv run python ci/build_dist.py` — Python via uv on a non-test
    file (CI script), allowed."""
    assert _check("uv run python ci/build_dist.py") is None

"""Tests for `ci/perf_local.py`'s `cargo` / `fmt` / `clippy` / `test` /
`shell` subcommands (issue #477).

These pin the argv shape of the `docker run` command each subcommand
produces — the named volumes, the rustup-state volume, the right
working directory, and the bash-vs-entrypoint wiring for the
component-install pre-step.

The tests are pure-Python: they import the perf_local.py module and
call its helpers directly, never invoking `docker`. Runs in any
Python 3.10+ environment without touching the host's Rust toolchain
or zccache daemon (which is the point of #477 in the first place).
"""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[2]
PERF_LOCAL_PATH = REPO_ROOT / "ci" / "perf_local.py"


def _load_perf_local():
    spec = importlib.util.spec_from_file_location("perf_local", PERF_LOCAL_PATH)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["perf_local"] = module
    spec.loader.exec_module(module)
    return module


@pytest.fixture(scope="module")
def pl():
    return _load_perf_local()


# ── build_zccache_docker_cmd: volume + working-dir invariants ──────────────


def test_build_cmd_mounts_named_target_volume(pl) -> None:
    cmd = pl.build_zccache_docker_cmd(entrypoint="cargo", cargo_args=["--version"])
    joined = " ".join(cmd)
    assert f"{pl.VOLUME_TARGET_ZCCACHE}:/target" in joined
    assert f"{pl.VOLUME_CARGO_HOME_ZCCACHE}:/cargo-home" in joined
    assert f"{pl.VOLUME_RUST_STATE}:/root/.rustup" in joined


def test_build_cmd_sets_working_dir_to_src(pl) -> None:
    cmd = pl.build_zccache_docker_cmd(entrypoint="cargo", cargo_args=["--version"])
    # Find `-w` and check the next arg is `/src` — otherwise cargo
    # would resolve workspace-relative paths against the container's
    # default CWD which would silently break recipes.
    idx = cmd.index("-w")
    assert cmd[idx + 1] == "/src"


def test_build_cmd_default_src_mode_is_readonly(pl) -> None:
    cmd = pl.build_zccache_docker_cmd(entrypoint="cargo", cargo_args=["--version"])
    src_mount = next(arg for i, arg in enumerate(cmd) if i > 0 and ":/src" in arg)
    assert src_mount.endswith(":/src:ro"), (
        f"expected default ro mount of /src, got {src_mount!r}"
    )


def test_build_cmd_can_request_readwrite_src_for_fmt_fix(pl) -> None:
    cmd = pl.build_zccache_docker_cmd(
        src_mode="rw",
        entrypoint="cargo",
        cargo_args=["fmt", "--all"],
    )
    src_mount = next(arg for i, arg in enumerate(cmd) if i > 0 and ":/src" in arg)
    assert not src_mount.endswith(":ro"), (
        f"src_mode='rw' must NOT carry the :ro suffix, got {src_mount!r}"
    )


def test_build_cmd_supports_bash_script_form(pl) -> None:
    cmd = pl.build_zccache_docker_cmd(
        bash_script="rustup component add rustfmt && cargo fmt --all -- --check",
    )
    # bash_script wraps via `/bin/bash -c <script>` — needed for the
    # rustup-component pre-install chain. Verify the right entrypoint
    # and `-c` flag are present.
    assert "--entrypoint" in cmd
    entry_idx = cmd.index("--entrypoint")
    assert cmd[entry_idx + 1] == "/bin/bash"
    # The script body is the LAST positional after `-c`.
    assert cmd[-2] == "-c"
    assert "rustup component add rustfmt" in cmd[-1]


def test_build_cmd_interactive_adds_it_flag(pl) -> None:
    cmd = pl.build_zccache_docker_cmd(
        interactive=True,
        entrypoint="/bin/bash",
    )
    # `-it` is required for interactive shell — without it docker
    # closes stdin immediately and the shell exits.
    assert "-it" in cmd


# ── per-subcommand argv shape (no docker invocation) ──────────────────────


def test_fmt_default_runs_check_mode(pl, monkeypatch: pytest.MonkeyPatch) -> None:
    captured: list[list[str]] = []
    monkeypatch.setattr(pl, "run_zccache_docker_cmd", lambda cmd: captured.append(cmd) or 0)
    pl.run_fmt([])
    assert captured, "run_fmt must invoke run_zccache_docker_cmd"
    cmd = captured[0]
    assert cmd[-2] == "-c"
    script = cmd[-1]
    assert "cargo fmt --all -- --check" in script
    assert "rustup component add rustfmt clippy" in script
    # Default fmt mode must NOT write back to the host repo.
    src_mount = next(arg for i, arg in enumerate(cmd) if i > 0 and ":/src" in arg)
    assert src_mount.endswith(":/src:ro")


def test_fmt_fix_drops_check_and_uses_rw_mount(pl, monkeypatch: pytest.MonkeyPatch) -> None:
    captured: list[list[str]] = []
    monkeypatch.setattr(pl, "run_zccache_docker_cmd", lambda cmd: captured.append(cmd) or 0)
    pl.run_fmt(["--fix"])
    cmd = captured[0]
    script = cmd[-1]
    assert "cargo fmt --all" in script
    assert "--check" not in script
    src_mount = next(arg for i, arg in enumerate(cmd) if i > 0 and ":/src" in arg)
    assert not src_mount.endswith(":ro"), (
        f"fmt --fix requires non-ro /src to rewrite files, got {src_mount!r}"
    )


def test_clippy_default_targets_zccache_lib_and_tests(pl, monkeypatch: pytest.MonkeyPatch) -> None:
    captured: list[list[str]] = []
    monkeypatch.setattr(pl, "run_zccache_docker_cmd", lambda cmd: captured.append(cmd) or 0)
    pl.run_clippy([])
    cmd = captured[0]
    script = cmd[-1]
    assert "cargo clippy" in script
    assert "-p zccache" in script
    assert "--lib" in script
    assert "--tests" in script
    assert "-D warnings" in script
    assert "rustup component add rustfmt clippy" in script


def test_clippy_forwards_extra_args_and_respects_user_separator(
    pl, monkeypatch: pytest.MonkeyPatch
) -> None:
    captured: list[list[str]] = []
    monkeypatch.setattr(pl, "run_zccache_docker_cmd", lambda cmd: captured.append(cmd) or 0)
    pl.run_clippy(["--workspace", "--all-targets", "--", "-A", "clippy::dbg_macro"])
    cmd = captured[0]
    script = cmd[-1]
    assert "--workspace" in script
    assert "--all-targets" in script
    # User supplied their own `--` separator, so the default
    # `-- -D warnings` tail must NOT be appended (would conflict).
    assert "-A clippy::dbg_macro" in script
    assert script.count("-D warnings") == 0


def test_test_subcommand_runs_lib_tests_with_pattern(
    pl, monkeypatch: pytest.MonkeyPatch
) -> None:
    captured: list[list[str]] = []
    monkeypatch.setattr(pl, "run_zccache_docker_cmd", lambda cmd: captured.append(cmd) or 0)
    pl.run_test(["fscache::metadata::tests::mtimes"])
    cmd = captured[0]
    # `cargo test --lib <pattern>` via the cargo entrypoint, NOT via bash.
    assert cmd[-3:] == ["test", "--lib", "fscache::metadata::tests::mtimes"]
    entry_idx = cmd.index("--entrypoint")
    assert cmd[entry_idx + 1] == "cargo"


def test_shell_subcommand_is_interactive_and_writes_to_repo(
    pl, monkeypatch: pytest.MonkeyPatch
) -> None:
    captured: list[list[str]] = []

    def fake_run(cmd, **_kwargs):
        captured.append(cmd)

        class _R:
            returncode = 0

        return _R()

    monkeypatch.setattr(pl.subprocess, "run", fake_run)
    pl.run_shell([])
    cmd = captured[0]
    assert "-it" in cmd
    src_mount = next(arg for i, arg in enumerate(cmd) if i > 0 and ":/src" in arg)
    assert not src_mount.endswith(":ro"), (
        f"shell needs rw /src for ad-hoc edits, got {src_mount!r}"
    )
    entry_idx = cmd.index("--entrypoint")
    assert cmd[entry_idx + 1] == "/bin/bash"


# ── volume-name regression guard ──────────────────────────────────────────


def test_volume_constants_are_named_not_paths(pl) -> None:
    """Issue #475's whole reason for being: target + cargo-home + rustup
    state must be NAMED Docker volumes (Linux ext4 in Docker's VFS), not
    host bind mounts (Windows + WSL2 9P rewrites mtimes per container
    start, defeating cargo's fingerprint). Pins each constant against
    the path-like patterns we explicitly DON'T want."""
    for name in (
        pl.VOLUME_TARGET_ZCCACHE,
        pl.VOLUME_TARGET_SOLDR,
        pl.VOLUME_CARGO_HOME_ZCCACHE,
        pl.VOLUME_CARGO_HOME_SOLDR,
        pl.VOLUME_RUST_STATE,
    ):
        assert isinstance(name, str), name
        assert not name.startswith(("/", ".", "~", "$")), (
            f"named volume must not look like a host path: {name!r}"
        )
        assert ":" not in name, f"named volume must not contain ':': {name!r}"

import os
import re
from pathlib import Path
from types import SimpleNamespace

import pytest

from ci import lint


def test_one_linux_dylint_job_gates_ordinary_pr_ci():
    workflow = (lint.SCRIPT_DIR / ".github/workflows/ci.yml").read_text(encoding="utf-8")
    job = workflow.split("\n  dylint:", 1)[1].split("\n  msrv:", 1)[0]
    assert "name: Dylint" in job
    assert "runs-on: ubuntu-latest" in job
    assert "dylint-platforms:" not in workflow
    assert "dylint-coverage:" not in workflow
    assert workflow.count("working-directory: ci/dylint-target-fixture") == 1
    assert "soldr dylint prepare" in job


def test_ci_dylint_job_lints_and_proves_every_os_target():
    # #1740: the late lints see only cfg-selected code, so the one Linux job
    # must also lint the Windows and macOS selections and prove each target
    # reports a planted violation.
    workflow = (lint.SCRIPT_DIR / ".github/workflows/ci.yml").read_text(encoding="utf-8")
    job = workflow.split("\n  dylint:", 1)[1].split("\n  msrv:", 1)[0]
    install = job.split("- name: Install Dylint nightly toolchain", 1)[1].split("\n      - name:", 1)[0]
    proof = job.split("- name: Prove custom-lint selection for every OS", 1)[1]
    for target in lint.DYLINT_OS_TARGETS.values():
        assert f"--target {target}" in install
        assert target in proof
    assert "uv run python -m ci.lint --dylint-only" in job


@pytest.mark.parametrize(
    ("platform", "expected"),
    [
        ("linux", ["x86_64-pc-windows-msvc", "aarch64-apple-darwin"]),
        ("win32", ["x86_64-unknown-linux-gnu", "aarch64-apple-darwin"]),
        ("darwin", ["x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc"]),
    ],
)
def test_dylint_cross_targets_cover_every_other_os(monkeypatch, platform, expected):
    monkeypatch.setattr(lint.sys, "platform", platform)
    assert lint.dylint_cross_targets() == expected


def test_windows_local_dylint_cannot_return_a_green_skip(monkeypatch):
    monkeypatch.setattr(lint, "os", SimpleNamespace(name="nt"))
    assert not lint.skip_dylint_on_windows()


def test_dylint_sources_do_not_set_a_dated_toolchain_globally():
    forbidden = re.compile(
        r"""set_var\s*\(\s*["']RUSTUP_TOOLCHAIN["']\s*,\s*["']nightly-\d{4}-\d{2}-\d{2}"""
    )
    violations = [
        str(path.relative_to(lint.SCRIPT_DIR))
        for path in (lint.SCRIPT_DIR / "dylints").rglob("*.rs")
        if forbidden.search(path.read_text(encoding="utf-8"))
    ]

    assert not violations, (
        "Dylint tests must inherit the front-door toolchain instead of mutating "
        f"process-global RUSTUP_TOOLCHAIN: {violations}"
    )


def test_dylint_workflow_rehydrates_the_pinned_toolchain_for_driver_builds():
    workflow = (lint.SCRIPT_DIR / ".github/workflows/ci.yml").read_text(
        encoding="utf-8"
    )
    dylint_job = workflow.split("\n  dylint:\n", 1)[1].split("\n  msrv:\n", 1)[0]

    assert "Configure Dylint driver Cargo shim" in dylint_job
    assert "nightly_bin=" in dylint_job
    assert 'nightly_toolchain="$(basename "$(dirname "${nightly_bin}")")"' in dylint_job
    assert 'subcommand="ru""stup"' in dylint_job
    assert 'export RUSTUP_TOOLCHAIN="%s"' in dylint_job
    assert 'exec soldr "${subcommand}" run "%s" cargo "$@"' in dylint_job
    assert 'echo "DYLINT_CARGO_SHIM=${shim_dir}" >> "${GITHUB_ENV}"' in dylint_job
    assert 'export PATH="${DYLINT_CARGO_SHIM}:${PATH}"' in dylint_job
    assert "--target x86_64-unknown-linux-gnu" in dylint_job
    library_tests = dylint_job.split("\n      - name: Run Dylint\n", 1)[0]
    assert "export PATH=\"${CARGO_HOME}/bin:${PATH}\"" not in library_tests
    assert "$(dirname \"${RUSTC}\")" not in dylint_job


def test_dylint_env_puts_selected_toolchain_first(monkeypatch):
    base_env = {"PATH": os.pathsep.join(["stable-bin", "other-bin"])}
    rustup = Path("host-shims") / "rustup"

    monkeypatch.setattr(lint, "self_build_env", lambda: base_env.copy())
    monkeypatch.setattr(
        lint,
        "which",
        lambda name: str(rustup) if name == "rustup" else None,
    )

    env = lint.dylint_env()

    assert env["RUSTUP_TOOLCHAIN"] == lint.DYLINT_TOOLCHAIN
    assert env["PATH"].split(os.pathsep)[0] == str(rustup.parent)


def test_ensure_dylint_aliases_copies_each_bare_library_once(monkeypatch, tmp_path):
    monkeypatch.setattr(lint, "SCRIPT_DIR", tmp_path)
    release = (
        tmp_path
        / "target"
        / "dylint"
        / "libraries"
        / lint.DYLINT_TOOLCHAIN
        / "release"
    )
    release.mkdir(parents=True)
    library = release / "libban_std_pathbuf.so"
    library.write_bytes(b"dylint fixture")

    assert lint.ensure_dylint_aliases()
    alias = release / f"libban_std_pathbuf@{lint.DYLINT_TOOLCHAIN}.so"
    assert alias.read_bytes() == b"dylint fixture"
    assert not lint.ensure_dylint_aliases()


def test_lint_dylint_only_uses_managed_fast_path_and_fails_closed(monkeypatch):
    monkeypatch.setattr(lint, "which", lambda _: "/tools/soldr")
    monkeypatch.setattr(lint, "self_build_env", lambda: {"RUSTFLAGS": "-D warnings", "RUSTUP_TOOLCHAIN": "1.95.0"})
    attempts = iter(
        [
            SimpleNamespace(returncode=0, stdout="", stderr=""),
            SimpleNamespace(returncode=1, stdout="", stderr="finding\n"),
        ]
    )
    calls = []

    def fake_run(command, **kwargs):
        calls.append((command, kwargs))
        return next(attempts)

    monkeypatch.setattr(lint.subprocess, "run", fake_run)

    assert lint.lint_dylint_only() == 1
    assert len(calls) == 2
    assert calls[0][0] == ["soldr", "dylint", "prepare"]
    assert calls[1][0] == ["soldr", "dylint", "--all", "--", "--workspace", "--lib", "--bins"]
    assert "RUSTFLAGS" not in calls[1][1]["env"]
    assert "RUSTUP_TOOLCHAIN" not in calls[1][1]["env"]


def test_lint_dylint_only_fails_closed_on_a_cross_target_finding(monkeypatch):
    # #1740: a Windows-only finding must fail local lint on a Linux host.
    monkeypatch.setattr(lint, "which", lambda _: "/tools/soldr")
    monkeypatch.setattr(lint, "self_build_env", lambda: {})
    monkeypatch.setattr(lint.sys, "platform", "linux")
    calls = []

    def fake_run(command, **kwargs):
        calls.append(command)
        failed = "x86_64-pc-windows-msvc" in command and "dylint" in command
        return SimpleNamespace(returncode=1 if failed else 0, stdout="", stderr="")

    monkeypatch.setattr(lint.subprocess, "run", fake_run)

    assert lint.lint_dylint_only() == 1
    workspace = ["soldr", "dylint", "--all", "--", "--workspace", "--lib", "--bins"]
    assert calls == [
        ["soldr", "dylint", "prepare"],
        workspace,
        [
            "soldr", "rustup", "target", "add",
            "--toolchain", lint.DYLINT_TOOLCHAIN,
            "x86_64-pc-windows-msvc", "aarch64-apple-darwin",
        ],
        [*workspace, "--target", "x86_64-pc-windows-msvc"],
    ]


def test_dylint_command_keeps_the_plugin_subcommand(monkeypatch):
    executable = "/opt/dylint"
    monkeypatch.setattr(lint, "which", lambda _: executable)

    assert lint.dylint_command() == [
        executable,
        "dylint",
        "--all",
        "--workspace",
    ]


def test_ensure_dylint_aliases_honors_configured_target_dir(tmp_path, monkeypatch):
    release_dir = (
        tmp_path
        / "dylint"
        / "libraries"
        / "nightly-2026-03-26-x86_64-unknown-linux-gnu"
        / "release"
    )
    release_dir.mkdir(parents=True)
    library = release_dir / "libexample.so"
    library.write_bytes(b"dylint")
    monkeypatch.setenv("CARGO_TARGET_DIR", str(tmp_path))

    assert lint.ensure_dylint_aliases()
    alias = release_dir / (
        "libexample@nightly-2026-03-26-x86_64-unknown-linux-gnu.so"
    )
    assert alias.read_bytes() == b"dylint"

    library.write_bytes(b"updated dylint")
    assert lint.ensure_dylint_aliases()
    assert alias.read_bytes() == b"updated dylint"


def _fail_if_cargo_runs(*_args, **_kwargs):
    raise AssertionError("the dependency guard must fail before any cargo command")


@pytest.mark.parametrize("argv", [[], ["--fix"]], ids=["lint", "stop-hook-fix"])
def test_lint_rejects_a_forbidden_dependency_before_ci(monkeypatch, capsys, argv):
    # #1518: a direct running-process dependency must fail locally (./lint and
    # the Stop hook's `ci.lint --fix`), not first in CI.
    violation = (
        "forbidden direct dependency: crates/x/Cargo.toml: [dependencies] "
        "running-process -> running-process (reach it only through kernal-api, #1518)"
    )
    monkeypatch.setattr(lint, "validate_release_metadata", lambda: None)
    monkeypatch.setattr(lint.check_kernal_api_baseline, "check", lambda: [violation])
    monkeypatch.setattr(lint, "run_cmd", _fail_if_cargo_runs)
    monkeypatch.setattr(lint.sys, "argv", ["lint", *argv])

    assert lint.main() == 1
    assert violation in capsys.readouterr().err


def test_lint_continues_to_cargo_when_the_dependency_guard_is_clean(monkeypatch):
    monkeypatch.setattr(lint, "validate_release_metadata", lambda: None)
    monkeypatch.setattr(lint.check_kernal_api_baseline, "check", lambda: [])
    # Hermetic: the ci/tests runner has no soldr on PATH, and cargo_command
    # resolves it before run_cmd would ever see the command.
    monkeypatch.setattr(lint, "cargo_command", lambda *args: ["cargo", *args])
    ran = []
    monkeypatch.setattr(
        lint, "run_cmd", lambda cmd: ran.append(cmd) or SimpleNamespace(returncode=0)
    )
    monkeypatch.setattr(lint.sys, "argv", ["lint", "--fix"])

    assert lint.main() == 0
    assert ran, "a clean guard must fall through to fmt and clippy"

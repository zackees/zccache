import os
import re
from pathlib import Path
from types import SimpleNamespace

from ci import lint


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
    assert 'nightly_toolchain="$(basename "${nightly_bin}")"' in dylint_job
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


def test_lint_dylint_only_retries_after_creating_aliases(monkeypatch):
    monkeypatch.setattr(lint, "skip_dylint_on_windows", lambda: False)
    monkeypatch.setattr(lint, "which", lambda _: "/tools/cargo-dylint")
    monkeypatch.setattr(lint, "ensure_dylint_components", lambda: 0)
    monkeypatch.setattr(lint, "dylint_command", lambda: ["cargo-dylint", "dylint"])
    monkeypatch.setattr(lint, "dylint_env", lambda: {"PATH": "/tools"})
    alias_results = iter([True])
    monkeypatch.setattr(lint, "ensure_dylint_aliases", lambda: next(alias_results))
    attempts = iter(
        [
            SimpleNamespace(returncode=1, stdout="", stderr="missing alias\n"),
            SimpleNamespace(returncode=0, stdout="", stderr=""),
        ]
    )
    calls = []

    def fake_run(command, **kwargs):
        calls.append((command, kwargs))
        return next(attempts)

    monkeypatch.setattr(lint.subprocess, "run", fake_run)

    assert lint.lint_dylint_only() == 0
    assert len(calls) == 2


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

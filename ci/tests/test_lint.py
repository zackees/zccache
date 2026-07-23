import os
from types import SimpleNamespace
from pathlib import Path

from ci import lint


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

import os
from pathlib import Path

from ci import lint


def test_dylint_env_puts_selected_toolchain_first(monkeypatch):
    base_env = {"PATH": os.pathsep.join(["stable-bin", "other-bin"])}
    rustc = Path("toolchains") / lint.DYLINT_TOOLCHAIN / "bin" / "rustc.exe"
    captured = {}

    monkeypatch.setattr(lint, "self_build_env", lambda: base_env.copy())

    def fake_check_output(command, **kwargs):
        captured["command"] = command
        captured["env"] = kwargs["env"]
        return f"{rustc}\n"

    monkeypatch.setattr(lint.subprocess, "check_output", fake_check_output)

    env = lint.dylint_env()

    assert captured["command"] == [
        "rustup",
        "which",
        "--toolchain",
        lint.DYLINT_TOOLCHAIN,
        "rustc",
    ]
    assert env["RUSTUP_TOOLCHAIN"] == lint.DYLINT_TOOLCHAIN
    assert env["PATH"].split(os.pathsep)[0] == str(rustc.resolve().parent)
    assert captured["env"] is env


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

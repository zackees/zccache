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

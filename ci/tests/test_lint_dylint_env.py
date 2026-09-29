import subprocess

from ci import lint


def _captured_envs(monkeypatch, configured: str | None) -> list[dict[str, str]]:
    envs: list[dict[str, str]] = []

    def fake_run(command, **kwargs):
        envs.append(kwargs["env"])
        return subprocess.CompletedProcess(command, 0)

    if configured is None:
        monkeypatch.delenv("SOLDR_DYLINT_CONFIGURED_TOOLCHAIN", raising=False)
    else:
        monkeypatch.setenv("SOLDR_DYLINT_CONFIGURED_TOOLCHAIN", configured)
    monkeypatch.setenv("SOLDR_DYLINT_TOOLCHAIN", "leaked-from-parent")
    monkeypatch.setattr(lint, "which", lambda name: f"/usr/bin/{name}")
    monkeypatch.setattr(lint, "dylint_cross_targets", lambda: [])
    monkeypatch.setattr(lint.subprocess, "run", fake_run)
    assert lint.lint_dylint_only() == 0
    assert envs
    return envs


def test_setup_soldr_run_leaves_the_dylint_scope_to_soldr(monkeypatch) -> None:
    """SOLDR_DYLINT_TOOLCHAIN marks a *nested* Dylint scope. Under setup-soldr it
    must be absent, or soldr never writes the success marker and setup-soldr
    skips the dylint-cache and dylint-output-cache saves."""
    for env in _captured_envs(monkeypatch, "nightly-2026-05-28"):
        assert "SOLDR_DYLINT_TOOLCHAIN" not in env


def test_local_run_still_pins_the_dylint_toolchain(monkeypatch) -> None:
    for env in _captured_envs(monkeypatch, None):
        assert env["SOLDR_DYLINT_TOOLCHAIN"] == lint.DYLINT_TOOLCHAIN

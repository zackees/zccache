from __future__ import annotations

import importlib.util
from pathlib import Path


MODULE_PATH = Path(__file__).parents[1] / "local_pre_pr.py"
SPEC = importlib.util.spec_from_file_location("local_pre_pr", MODULE_PATH)
assert SPEC and SPEC.loader
local_pre_pr = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(local_pre_pr)


def test_run_perf_local_uses_shared_harness(monkeypatch):
    calls: list[tuple[list[str], Path]] = []

    def fake_run(command, *, cwd, check):
        calls.append((command, cwd))

    monkeypatch.setattr(local_pre_pr.subprocess, "run", fake_run)
    local_pre_pr.run_perf_local("fmt")

    assert calls == [
        (
            ["uv", "run", "--no-project", "python", str(local_pre_pr.PERF_LOCAL), "fmt"],
            local_pre_pr.REPO_ROOT,
        )
    ]

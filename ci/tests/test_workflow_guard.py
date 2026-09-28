"""Tests for the #1760 PostToolUse workflow/cache-footprint hook."""

from __future__ import annotations

import importlib.util
import json
import subprocess
from pathlib import Path

import pytest


def _load_workflow_guard():
    module_path = Path(__file__).resolve().parents[1] / "hooks" / "workflow_guard.py"
    spec = importlib.util.spec_from_file_location("workflow_guard", module_path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


guard = _load_workflow_guard()

MACOS_COOK = """on: [push, pull_request]
jobs:
  mac:
    runs-on: macos-15
    steps:
      - uses: zackees/setup-soldr@4df8db93438594f50505574d9dc8117505d33362
        with:
          toolchain: 1.95.0
          cook-delta: false
          solo-toolchain-cache: false
"""


def _repo(tmp_path: Path) -> Path:
    subprocess.run(["git", "init", "-q", str(tmp_path)], check=True)
    (tmp_path / "README.md").write_text("scratch\n", encoding="utf-8")
    subprocess.run(["git", "-C", str(tmp_path), "add", "-A"], check=True)
    subprocess.run(
        ["git", "-C", str(tmp_path), "-c", "user.name=t", "-c", "user.email=t@t",
         "commit", "-qm", "init"],
        check=True,
    )
    return tmp_path


def _workflow(root: Path, text: str) -> Path:
    path = root / ".github" / "workflows" / "mac.yml"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")
    return path


@pytest.mark.parametrize(
    ("path", "watched"),
    [
        (".github/workflows/ci.yml", True),
        ("./.github/actions/build-target/action.yml", True),
        ("action.yml", True),
        ("ci/cache_cleanup_plan.js", True),
        ("ci/check_cache_footprint.py", True),
        ("crates/zccache/src/lib.rs", False),
        ("github/workflows/ci.yml", False),
    ],
)
def test_watched_paths(path: str, watched: bool) -> None:
    assert guard.is_watched(path) is watched


def test_bash_edit_of_a_bad_workflow_is_red(tmp_path: Path, capsys) -> None:
    """A sed/heredoc edit leaves no file_path; git status must catch it."""
    root = _repo(tmp_path)
    _workflow(root, MACOS_COOK)
    payload = {"tool_name": "Bash", "tool_input": {"command": "sed -i ... mac.yml"}}

    assert guard.main(json.dumps(payload), root) == 2
    assert "cook bases are Linux-only" in capsys.readouterr().err


def test_edit_payload_triggers_the_guard(tmp_path: Path) -> None:
    root = _repo(tmp_path)
    path = _workflow(root, MACOS_COOK)
    payload = {"tool_name": "Edit", "tool_input": {"file_path": str(path)}}
    assert guard.main(json.dumps(payload), root) == 2


def test_fixed_workflow_is_green(tmp_path: Path) -> None:
    root = _repo(tmp_path)
    _workflow(root, MACOS_COOK + "          prebuild-deps: none\n")
    assert guard.main(json.dumps({"tool_name": "Bash", "tool_input": {}}), root) == 0


def test_unrelated_changes_skip_the_guard(tmp_path: Path, monkeypatch) -> None:
    root = _repo(tmp_path)
    (root / "notes.txt").write_text("unrelated\n", encoding="utf-8")

    def fail(_root):
        raise AssertionError("guard must not run for unrelated changes")

    monkeypatch.setattr(guard, "run_guard", fail)
    assert guard.main(json.dumps({"tool_name": "Bash", "tool_input": {}}), root) == 0
    assert guard.main("not json", root) == 0


def test_hook_is_registered_for_edits_and_bash() -> None:
    settings = json.loads(
        (Path(__file__).resolve().parents[2] / ".claude/settings.json").read_text(
            encoding="utf-8"
        )
    )
    commands = {
        entry["matcher"]: [hook["command"] for hook in entry["hooks"]]
        for entry in settings["hooks"]["PostToolUse"]
    }
    expected = "uv run --no-project --with pyyaml python ci/hooks/workflow_guard.py"
    assert expected in commands.get("Edit|Write", [])
    assert expected in commands.get("Bash", [])

# Agent Hooks

Python scripts invoked by Claude Code and Codex hooks. Claude Code loads
`.claude/settings.json`; Codex loads `.codex/hooks.json`.

All hooks are executed via `uv run` to ensure consistent Python environment.

| Hook | Event | Purpose |
|---|---|---|
| `tool_guard.py` | PreToolUse (shell) | Blocks bare cargo/rustc/python/pip; routes Rust through soldr and Python through uv. |
| `readme_guard.py` | PostToolUse (Edit/Write) | Every directory needs a README.md. |
| `loc_guard.py` | PostToolUse (Edit/Write) | Warns above 1,000 LOC, blocks above 1,500. |
| `workflow_guard.py` | PostToolUse (Edit/Write, Bash) | Runs `ci/check_cache_footprint.py` when a workflow, action or the cache planner changes, including edits made through Bash (#1760). |
| `check-on-start.py` | SessionStart | Captures the git fingerprint. |

Run the CI contract suite locally by invoking `pytest` directly
(`tool_guard.py` blocks `python -m pytest <tests path>` but allows this form):

```bash
PYTHONPATH=. uv run --no-project --python 3.13 --with pytest --with pyyaml --with pillow pytest -q ci/tests
```

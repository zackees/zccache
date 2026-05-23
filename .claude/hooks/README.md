# Claude Code Hooks

Hook scripts that enforce project standards automatically.

- **tool_guard.py** - PreToolUse: blocks bare Rust commands (must use `soldr`)
- **lint.py** - PostToolUse: auto-formats + runs clippy on edited .rs files
- **readme_guard.py** - PostToolUse: ensures every directory has a README.md
- **check-on-start.py** - SessionStart: captures git fingerprint
- **Stop hook** - `soldr cargo run -p zccache --bin zccache-ci`: runs full workspace lint + tests

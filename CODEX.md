Read [CLAUDE.md](./CLAUDE.md) first. The following conventions are mandatory in this repo.

## Rust commands

- Do not run bare `cargo`, `rustc`, or `rustfmt`.
- Always use the project-root trampolines (`./_cargo`, `./_rustc`, `./_rustfmt`) or `soldr <tool>` directly. Both forms resolve through [soldr](https://github.com/zackees/soldr), which calls `rustup which` to pick the rustup-managed toolchain.
- In this Windows environment, the repo is using the rustup-managed `x86_64-pc-windows-msvc` toolchain pinned by `rust-toolchain.toml`. Do not assume GNU or try to find `gcc`. soldr also enforces MSVC on Windows by default.
- The trampolines set up the correct rustup environment and are the contract enforced by repo hooks. The `./_cargo` path uses `soldr --no-cache cargo` so the previous bare-cargo semantics are preserved.

Examples:

```bash
./_cargo check --workspace --all-targets
./_cargo test -p zccache-download
./_cargo build -p zccache-cli
./_cargo fmt --all
```

If invoking from PowerShell on Windows and the trampoline is a bash script, use:

```powershell
bash ./_cargo check -p zccache-download-client
bash ./_cargo test -p zccache-download
```

## Python commands

- Do not run bare `python` or `pip`.
- Always use `uv run ...` or `uv pip ...`.
- Python is only for CI scripts, packaging, and hooks. Runtime logic, tests, and benchmarks belong in Rust unless there is an explicit exception.

## Test entrypoints

- Prefer `./test` for repo-standard test execution.
- Use `./test --integration` for ignored integration tests.
- Use `./test --full` for the larger suite.
- Use `./_cargo test ...` when you need targeted crate/test execution.

## Practical rule

- When working in this repo, prefer repo entrypoints over generic tool invocations. If there is a local wrapper or script, use it.

# ban_raw_subprocess_in_daemon

This lint forbids the daemon production code from spawning child processes
via `std::process::Command::{spawn, output, status}` or
`tokio::process::Command::{spawn, output, status}` directly. Every child
process the daemon launches **must** go through the blessed helpers in
`crates/zccache-daemon/src/process.rs`:

| Use case          | Helper                                |
| ----------------- | ------------------------------------- |
| Synchronous spawn | `command_output_with_priority(cmd, p)` |
| Async spawn       | `tokio_command_output_with_priority(cmd, p).await` |

## Why

The daemon is launched detached (no console attached). On Windows, a
console-subsystem child spawned from a console-less parent without the
`CREATE_NO_WINDOW` flag triggers the OS to allocate a fresh console
window for the child — a visible flash per cache-miss compile in the
`soldr → cargo → rustc → zccache-cli → daemon → rustc` call chain.

The helpers in `process.rs` apply `CREATE_NO_WINDOW` (and consistent
stdio piping, Job Object containment, child-priority adjustment). Bypassing
them silently regresses one or more of those invariants. This lint catches
the bypass at compile time.

## Scope

The lint fires only on source files under `crates/zccache-daemon/src/`.
Other crates (cli, ci, fingerprint, …) are out of scope: the cli already
has its own sanitized spawn (`spawn_daemon_windows::spawn_daemon_sanitized`)
for the daemon-launching hop, and the other crates don't spawn compilers.

## Allowlist

`src/allowlist.txt` lists the daemon source files that are exempt:

- `process.rs` — the blessed helpers themselves call `.spawn()` /
  `.output()` internally; that's the entire point.
- `server.rs`, `lineage.rs` — contain `#[cfg(test)]` modules with
  `Command::new("chmod" / "echo").status()` / `.output()`. Test code
  doesn't ship in the production binary so the console-flash bug
  doesn't apply there. (A future improvement could detect `#[cfg(test)]`
  scope programmatically and remove these entries.)

## Adding a new spawn site

If you genuinely need a new daemon-side spawn pattern that the existing
helpers don't cover, **extend `process.rs`** with a new helper that
applies the same defaults (`CREATE_NO_WINDOW`, piped stdio, Job Object
attach, priority) and route the call site through it. Do not add the file
to the allowlist.

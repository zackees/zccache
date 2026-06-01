# `wrap` — compiler/linker/archiver wrapper

Implements `zccache <compiler> <args...>` and `zccache wrap`. The facade in
[`../wrap.rs`](../wrap.rs) routes each invocation to one of the submodules
here based on tool family and environment.

## Files

- **`env.rs`** — environment policy: `ZCCACHE_DISABLE`, `ZCCACHE_STRICT_PATHS`,
  client-env filtering applied to every IPC request.
- **`ipc.rs`** — request builders and response handling for `Compile` and
  `LinkEphemeral`. Owns the per-request retry policy.
- **`passthrough.rs`** — direct-exec paths used when the cache is disabled or
  the tool is unsupported. Captures + releases the wrapper's CWD on Windows
  so the build dir is not pinned by a kernel handle (issue #555 / #134).
- **`routing.rs`** — classifies an argv into `Formatter | LinkOrArchive |
  Compile` without doing the full parse.
- **`rustfmt.rs`** — format-cache wrapper for rustfmt: skip files whose
  content hash is already cached (avoids mtime churn that triggers downstream
  rebuilds).
- **`tool_resolution.rs`** — resolves bare tool names (`clang++`, `ar`, ...)
  to absolute paths via `PATH`, with the policy decision about when to leave
  the original spelling alone for the daemon to error on.

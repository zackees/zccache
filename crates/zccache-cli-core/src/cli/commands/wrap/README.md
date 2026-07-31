# `wrap` — compiler/linker/archiver wrapper

Implements `zccache <compiler> <args...>` and `zccache wrap`. The facade in
[`../wrap.rs`](../wrap.rs) routes each invocation to one of the submodules
here based on tool family and environment.

## Files

- **`diag.rs`** — opt-in CWD/argv diagnostic. When `ZCCACHE_DIAG_CWD` is set
  to any non-empty, non-`0` value, each wrapper invocation emits one
  tab-separated `ZCCACHE_DIAG_CWD` line to stderr before any CWD mutation,
  carrying `pid`, `cwd`, `tmp`, `argv0`, and the raw `args`. Used to
  diagnose cases (issue #683) where the request reaching the daemon carries
  an unexpected CWD — typically because an outer shim chdir'd before exec.
- **`env.rs`** — environment policy: `ZCCACHE_DISABLE`, `ZCCACHE_STRICT_PATHS`,
  client-env filtering applied to every IPC request.
- **`ipc.rs`** — request builders and response handling for `Compile` and
  `LinkEphemeral`. Owns the per-request retry policy.
- **`passthrough.rs`** — direct-exec paths used when the cache is disabled or
  the tool is unsupported. Captures + releases the wrapper's CWD on Windows
  so the build dir is not pinned by a kernel handle (issue #555 / #134).
  Since #1170 these are the *sanctioned* bypasses only; a daemon failure no
  longer routes here — see `unavailable.rs`.
- **`routing.rs`** — classifies an argv into `Formatter | LinkOrArchive |
  Compile` without doing the full parse.
- **`rustfmt.rs`** — correctness-first rustfmt wrapper. Normal Cargo/rustfmt
  invocations always run because rustfmt recursively discovers child modules
  that are absent from the explicit argv. Invocations that explicitly set
  `skip_children=true` are non-recursive and may skip source files whose
  content hash is already known formatted.
- **`unavailable.rs`** — the refusal path when the daemon cannot be reached
  before dispatch (issue #1170). Exits **125** (wrapper-infrastructure
  failure, distinct from a compiler's 1/2 so CI can classify it without
  parsing stderr), emits `zccache[err][D]:` plus a durable
  `wrapper-daemon-unavailable` lifecycle event. This replaces the old silent
  local fallback, which mirrored the tool's exit code and so let a daemon
  outage present as a green build. The only sanctioned bypasses remain
  `ZCCACHE_DISABLE` and `ZCCACHE_PROBE_BYPASS`.
- **`tool_resolution.rs`** — resolves bare tool names (`clang++`, `ar`, ...)
  to absolute paths via `PATH`, with the policy decision about when to leave
  the original spelling alone for the daemon to error on.

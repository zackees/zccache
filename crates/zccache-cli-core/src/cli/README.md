# Source

Source files for `zccache-cli`.

## Daemon acquisition and recovery (#1161, #1170)

There is **one** implementation of "get me a working daemon":
`runtime::ensure_daemon`. `commands/daemon.rs` used to carry a second,
unhardened copy that the wrapper hot path imported; #1161 deleted it. Do not
reintroduce a parallel stack — every kill-correctness fix has to hold on the
path that actually runs per compile.

- **`runtime.rs`** — the ladder itself: probe → classify → identity-scoped
  cleanup → respawn → readiness. Every kill is bound to the instance the
  caller failed to talk to, captured *before* the exchange
  (`current_daemon_instance`); a kill that cannot name its target is refused.
- **`recovery.rs`** — the bounds around the ladder. A total deadline
  (`ZCCACHE_RECOVERY_BUDGET_MS`, default 30 s; `0` disables the cap) and a
  cross-invocation breaker. The wrapper is a fresh process per translation
  unit, so the breaker is a file (`<daemon-lock>.spawn-failed`): the first
  exhaustion writes it, later invocations inside the cool-down (60 s,
  doubling, capped at 10 min) fail immediately with the *original* reason, and
  any success clears it. Without it a 1000-TU build against a dead daemon paid
  the full ladder 1000 times.

When recovery is exhausted the wrapper hard-errors — see
`commands/wrap/unavailable.rs` for the exit code and event contract. There is
no silent uncached fallback.

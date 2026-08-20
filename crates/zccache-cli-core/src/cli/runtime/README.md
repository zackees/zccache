# `runtime` — daemon acquisition, deployment and spawn

Split out of a single `runtime.rs` when it crossed the 1.5K-LOC guard. The cut
follows the question each half answers:

- **`mod.rs`** — *should we have a daemon, and which one?* The version probe,
  the identity-bound stop (#1161), the bounded recovery ladder, and the
  spawn-slot single-flight. `ensure_daemon` is the only entry point; do not add
  a second implementation elsewhere — a duplicate in `commands/daemon.rs` is
  what let #1161's fixes miss the wrapper hot path entirely.
- **`deploy.rs`** — *where does the binary live and is it the one we think?*
  Version-rooted self-materialization, content verification before execution
  (#1172), the deploy directory's permissions, spawn-log allocation and GC, and
  the spawn itself.
- **`tests.rs`** — lifecycle decision, recovery, drain, and identity-gate tests
  kept separate so the production module remains below the source-size ceiling.

`mod.rs` re-exports `deploy`'s public items, so the paths callers use are
unchanged.

# Connection

Per-client IPC connection dispatch loop. Split out of a single 1590-LOC
`connection.rs` (issue #1154 phase-0, `crates/CLAUDE.md` § File-size
discipline) into:

- `mod.rs` — `handle_connection` (the read/dispatch/send loop), `ResponseWire`,
  `PendingJournalContext`, `guarded_dispatch` (issue #967 disconnect-cancel
  guard), `log_client_cancelled`, `send_response_for_wire`,
  `session_phase_profile`. Owns the journal-entry bookkeeping that wraps each
  dispatched request.
- `dispatch.rs` — `dispatch_request` / `DispatchOutcome`: the per-`Request`
  match arm that used to be inlined in `handle_connection`, plus
  `compile_response_for_session`.
- `attribution.rs` — cache-miss attribution: `compile_miss_reason`,
  `append_unknown_miss_warning`, `redacted_args_preview`,
  `derive_approx_spans`. Named `attribution` (not `miss_reason`) so it
  doesn't shadow the glob-imported `compile_journal::miss_reason` constants
  module. `pub(in crate::daemon::server)` so the embedded compile path
  (`server/embedded.rs`) and `server/tests/` unit tests share the same
  attribution logic as the IPC path (soldr#1286).

Unit tests for this module live under `server/tests/` (`connection_ipc.rs`,
`connection_self_profile.rs`, `connection_disconnect.rs`) per the repo's
`tests/` subdirectory convention.

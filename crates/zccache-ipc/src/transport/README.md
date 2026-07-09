# `ipc::transport`

Platform-abstracted IPC transport: length-prefixed bincode (and prost / running-process
`Frame` envelopes) over Unix domain sockets on Unix and named pipes on Windows.

Split from the original `transport.rs` for the 1,000-LOC discipline (see CLAUDE.md).
Public path `crate::transport::<Name>` is preserved via re-exports in
[`mod.rs`](mod.rs).

## Files

- [`mod.rs`](mod.rs) — `DEFAULT_CLIENT_RECV_TIMEOUT`, `IpcConnection` (server-side
  on Windows, both sides on Unix), `IpcListener`, `unique_test_endpoint`,
  and platform re-exports.
- [`framing.rs`](framing.rs) — Shared bincode / dual-wire prost decode loops and
  buffered-read helpers (`recv_bincode_loop`, `recv_wire_loop`, `read_next_chunk`,
  `ensure_buffered`, `decode_response_wire`).
- [`probe.rs`](probe.rs) — `try_serve_backend_handle_probe` and supporting
  helpers for the running-process `BackendHandle` identity handshake.
- [`unix.rs`](unix.rs) — `#[cfg(unix)]` client `connect` and
  `IpcConnection::from_unix_stream`.
- [`windows.rs`](windows.rs) — `#[cfg(windows)]` `IpcClientConnection`, client
  `connect` with `ERROR_PIPE_BUSY` backoff, Windows accept path (pool, emergency
  create, issue #666 recovery).
- [`tests.rs`](tests.rs) — Async unit tests for the transport.

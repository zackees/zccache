# `ipc::transport`

Product IPC framing over the neutral `zccache-platform::ipc` byte-stream
facade. Unix sockets, Windows named pipes, endpoint security, peer identity,
connection retry, and listener pooling are owned by `zccache-platform`.

Public paths under `crate::transport::<Name>` remain stable.

## Files

- [`mod.rs`](mod.rs) — `IpcConnection`, `IpcListener`, product timeouts, and
  endpoint helpers.
- [`framing.rs`](framing.rs) — shared bincode and prost decode loops plus
  buffered-read helpers.
- [`probe.rs`](probe.rs) — running-process backend-handle probe framing.
- [`tests.rs`](tests.rs) — product framing and connection-policy tests over the
  neutral platform transport.

# zccache-ipc

Platform IPC endpoint discovery: Unix domain sockets or Windows named pipes.

The default `IpcConnection::send` / `recv` path remains the v15 bincode daemon
wire. The running-process#234 migration hook lives beside it:
`send_prost` writes an explicit v16 prost frame, and `recv_wire` dispatches
incoming frames by protocol-version header so a server can accept either v15
bincode or v16 prost without breaking old clients.

`daemon_control_roundtrip` is the only live client-side selector today. It
honors `ZCCACHE_DAEMON_WIRE` for `Ping`, `Status`, `Shutdown`, and cache
`Clear`; unset/`auto` tries v16 prost first and retries v15 bincode when an
older daemon rejects the frame. The v16 control path accepts either v16 prost
control replies or v15 bincode replies from compatibility daemons. The daemon
also accepts direct v16 prost `ReleaseWorktreeHandles` requests and sends the
matching v16 prost response, but there is no high-level client selector for
that path yet. Compile, session, artifact lookup/store, fingerprint,
generic-tool, and download-daemon requests still use v15 bincode directly.

`tests/daemon_wire_protocol_version.rs` includes the explicit previous-release
compatibility harness: a v15-only daemon rejects the first v16 prost control
frame, returns a v15 bincode mismatch response, and the public auto client
retries the same control request as v15 bincode.

Minimal running-process adoption is intentionally separate from the full broker
rollout. The direct zccache daemon now records
`daemon.running-process.json` beside its lock file and answers the reserved
`BackendHandle` endpoint probe on the existing IPC endpoint. This lets callers
verify the current daemon through `running_process::broker::BackendHandle`
without requiring a `.servicedef`, broker-client routing, default-on rollout,
or the remaining protobuf message-family conversions. The daemon pre-bind
probe uses that BackendHandle identity when present and falls back to the
legacy raw-connect probe for older daemons that have not written the identity
file yet.

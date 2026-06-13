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
file yet. `RUNNING_PROCESS_DISABLE=1` skips the BackendHandle probe and uses
that same legacy raw-connect fallback.

`broker.rs` wires the frozen
`running_process::broker::adopt::AsyncBrokerSession::adopt` one-call recipe
(zackees/running-process#435) in front of the daemon client connect
(`connect_daemon`). `adopt` runs the Hello negotiation (service_name
`"zccache"`, protocol min/max = 1, client_lib_name `"running-process"`,
wanted_version = the zccache daemon version) on a blocking worker and returns
the broker-selected backend endpoint. The lane is opt-in
(`ZCCACHE_BROKER_CONNECT=1`, or the upstream TEST-ONLY
`RUNNING_PROCESS_FAKE_BACKEND` seam, which still dials directly via
`connect_local_socket`); `RUNNING_PROCESS_DISABLE=1` bypasses it entirely, and
any broker-side failure falls back silently to the direct connect. Typed
broker refusals are classified through `RefusalKind` into the local
`BrokerRefusal` enum (`classify_adopt_error`). The negotiated connection
resolves the endpoint only — the data connection still uses zccache's tokio
transport and wire lanes unchanged.

`manifest.rs` publishes the zccache `CacheManifest` into the running-process
central registry at daemon startup via the frozen `CacheManifestBuilder`
(zackees/running-process#433), mapping the five zccache cache roots onto the
v1 `CacheRootKind` taxonomy (artifact→`CacheData`, index→`CacheIndex`,
log→`CacheLogs`, lock→`CacheLocks`, temp→`CacheTmp`). Publishing is
best-effort and honors `RUNNING_PROCESS_DISABLE=1`. The companion
`cli/commands/service_definition.rs` registers the `SHARED_BROKER`
`ServiceDefinition` through `ServiceDefinitionBuilder::shared_broker`.

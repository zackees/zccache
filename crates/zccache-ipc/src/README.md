# zccache-ipc

Platform IPC endpoint discovery: Unix domain sockets or Windows named pipes.

The low-level `IpcConnection::send` / `recv` primitives use the v16 prost wire.
`send_prost` writes the same length-prefixed prost frame, and `recv_wire`
dispatches incoming prost frames by their protocol-version header.

High-level non-streaming requests use `full_family_roundtrip`; session,
generic-exec, artifact, and fingerprint callers therefore use v16 prost in
unset/`auto` mode. `daemon_control_roundtrip` applies the same policy to
control traffic, while the wrapper's streaming compile/link path preserves
progress frames under the same selection. Explicit prost and running-process
FrameV1 selections remain strict; retired wire values return a clear
unsupported-value error. The separate download-daemon protocol is not part of
this wire migration.

`full_family_roundtrip_classified` additionally retains connect-versus-delivery
failure phase for idempotent callers.

`full_family.rs` owns this prost-only selection and failure-phase API; `lib.rs`
re-exports its public entry points.

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

`ownership.rs` owns the endpoint-scoped daemon ownership records (#1904): the
lock file and the identity sidecar above. Both fold a `stable_path_id` scope
segment for any endpoint other than `default_endpoint()`, so a daemon on
`ZCCACHE_ENDPOINT=/tmp/custom.sock` owns its own records and cannot contend
with — or be killed through — the default daemon's. The default endpoint keeps
its historical filenames byte-for-byte. Every helper takes the endpoint it was
asked about; the no-argument forms (`check_running_daemon`,
`read_backend_identity`, `force_kill_verified_daemon`, …) are
default-endpoint-only wrappers. A kill pairs a PID from one endpoint's lock with
an identity from that same endpoint's sidecar, so scoping only the lock is not
sufficient.

`broker.rs` wires the frozen
`AsyncBrokerSession::adopt` one-call recipe (re-exported through
`running_process::broker::protocol_v2::client_compat` per zccache#782
slice 25; underlying impl per zackees/running-process#435) in front of
the daemon client connect
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

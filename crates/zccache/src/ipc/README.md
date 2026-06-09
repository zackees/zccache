# zccache-ipc

Platform IPC endpoint discovery: Unix domain sockets or Windows named pipes.

The default `IpcConnection::send` / `recv` path remains the v15 bincode daemon
wire. The running-process#234 migration hook lives beside it:
`send_prost` writes an explicit v16 prost frame, and `recv_wire` dispatches
incoming frames by protocol-version header so a server can accept either v15
bincode or v16 prost without breaking old clients.

`daemon_control_roundtrip` is the only live client-side selector today. It
honors `ZCCACHE_DAEMON_WIRE` for `Ping`, `Status`, and `Shutdown`; unset/`auto`
tries v16 prost first and retries v15 bincode when an older daemon rejects the
frame. The v16 control path accepts either v16 prost control replies or v15
bincode replies from compatibility daemons. All non-control zccache daemon
requests still use v15 bincode directly.

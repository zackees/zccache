//! Daemon wire-protocol integration tests (#1526).
//!
//! Covers frame v1, the dispatcher, prost round-trips, protocol
//! versioning, and IPC timeouts. Each former top-level `tests/*.rs` file
//! is a module here, so the category links one test executable instead of
//! 6. Test IDs are `wire::<module>::<test_name>`.
//!
//! Per-module lint allows and `#![cfg(...)]` gates stay as the inner
//! attributes at the top of each file, so nothing is widened to a
//! common denominator.

mod daemon_wire_dispatcher;
mod daemon_wire_frame_v1_test;
mod daemon_wire_perf_scaffold;
mod daemon_wire_prost_roundtrip;
mod daemon_wire_protocol_version;
mod ipc_timeout;

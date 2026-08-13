//! Neutral local-transport mechanics: Stream/Listener/Endpoint/PeerIdentity
//! over Unix sockets or Windows named pipes, owner-only socket/pipe
//! security, and native endpoint retirement.
//!
//! zccache-ipc keeps the product layer — endpoint versioning/namespaces,
//! framing, wire compatibility, broker routing, timeouts, and diagnostics.
//! Populated in the IPC phase (#1368); empty index until then.

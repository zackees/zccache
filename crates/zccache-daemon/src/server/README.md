# Server

IPC server entry point and request handlers for the daemon. `mod.rs` owns
`DaemonServer`, `SharedState`, and the main loop; the other modules split out
self-contained units (handlers, persistence, key normalization, WAL, tests) so
no single file is too large to navigate.

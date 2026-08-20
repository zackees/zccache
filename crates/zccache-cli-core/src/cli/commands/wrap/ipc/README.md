# Wrapper IPC tests

This directory holds the focused tests split from `../ipc.rs` so the wrapper
IPC implementation remains within the repository source-file size limit.

`tests.rs` covers compile/link retry phases, wire-selection fallback, request
construction, and terminal response handling. Production code remains in
`../ipc.rs`.

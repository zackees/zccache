# Wrapper IPC submodules

This directory holds the focused tests and submodules split from `../ipc.rs`
so the wrapper IPC implementation remains within the repository source-file
size limit.

`tests.rs` covers compile/link retry phases, wire-selection fallback, request
construction, and terminal response handling. `lost_request.rs` classifies the
requests the daemon lost with no tool left running and logs them as
`wrapper-no-verdict`. The rest of the production code remains in `../ipc.rs`.

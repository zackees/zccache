# zccache-cli-core sources

- `lib.rs` — crate root: re-aliases the subsystem crates (`core`, `ipc`, …) and
  `daemon` (from `zccache-daemon-core`), then declares the CLI subsystem modules.
- `cli/` — subcommands, compiler wrapper mode, and client (moved from `zccache`).
- `download_client/` — client for the download-cache daemon (CLI-only).
- `download_daemon/` — download-cache daemon logic (CLI-only).

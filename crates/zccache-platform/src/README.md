# zccache-platform — source layout

- `lib.rs` — the **only** host-selection site in the workspace:
  `cfg_select!` over `target_os`, aliasing the selected concrete tree to
  `crate::platform_imp`. No fallback arm.
- `platform.rs` + `platform/` — neutral facade. Contains no host cfg, no
  native imports, and never names `platform_win`/`platform_linux`/
  `platform_macos`. The only bridge to the selected implementation is
  `crate::platform_imp`.
- `platform_{win,linux,macos}.rs` + `platform_{win,linux,macos}/` — private
  concrete trees. All host cfg and native APIs (`std::os::*`, `libc`,
  `windows-sys`) live here. Linux and macOS remain separate trees even where
  they share call sites.

Each capability phase (fs → ipc → process → executable/host) fills in its
facade leaf and concrete leaves together; until then the facades are empty
indexes by design.

# platform_macos — concrete macOS tree

macOS-only implementation leaves (`process`, `fs`, `ipc`, `executable`,
`host`) selected by the `cfg_select!` in `src/lib.rs`. Native APIs (`libc`,
`std::os::unix::*`) are legal here and only here.

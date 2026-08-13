# platform_win — concrete Windows tree

Windows-only implementation leaves (`process`, `fs`, `ipc`, `executable`,
`host`) selected by the `cfg_select!` in `src/lib.rs`. Native APIs
(`windows-sys`, `std::os::windows::*`) are legal here and only here.

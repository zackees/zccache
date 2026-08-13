//! Concrete Windows host tree. Private — never named by any other crate.
//!
//! All Windows host cfg and native APIs (`std::os::windows::*`,
//! `windows-sys`) live here and in `platform_win/`. Populated per capability
//! phase; empty until then.

pub mod fs;
pub mod ipc;
pub mod process;

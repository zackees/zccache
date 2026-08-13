//! Concrete macOS host tree. Private — never named by any other crate.
//!
//! All macOS host cfg and native APIs (`std::os::unix::*`, `libc`) live
//! here and in `platform_macos/`. Linux and macOS remain separate trees even
//! where they share call sites. Populated per capability phase; empty until
//! then.

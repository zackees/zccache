//! Neutral facade root.
//!
//! This file and the `platform/` tree contain **no host cfg and no native
//! imports**. Facade leaves expose neutral types and operations; the concrete
//! host implementation is reached only through `crate::platform_imp`, which
//! `lib.rs` selects exactly once.

pub mod executable;
pub mod fs;
pub mod host;
pub mod ipc;
pub mod process;

//! `zccache-monocrate` — transitional absorber crate. See README and issue #365.
//!
//! Each `pub mod` below corresponds to a former workspace crate of the same
//! name (`zccache-core` → [`core`], `zccache-hash` → [`hash`], etc.). New code
//! should `use zccache_monocrate::<module>::*` instead of the legacy
//! `zccache_<module>::*` paths, which are being deleted wave by wave.

pub mod artifact;
pub mod compiler;
pub mod core;
pub mod depgraph;
pub mod fingerprint;
pub mod fscache;
pub mod gha;
pub mod hash;
pub mod ipc;
pub mod protocol;
pub mod symbols;
pub mod watcher;

#[cfg(feature = "test-support")]
pub mod test_support;

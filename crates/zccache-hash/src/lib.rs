//! Hashing utilities and effect-free request-key encoding for zccache.
//!
//! Disable default features for the encoder-only surface. Native callers retain
//! the existing content hashing and cache-key APIs through the default feature.

pub mod request_fingerprint;

#[cfg(feature = "native")]
pub mod cache_key;
#[cfg(feature = "native")]
pub mod link_cache_key;
#[cfg(feature = "native")]
mod native;
#[cfg(feature = "native")]
pub use native::*;

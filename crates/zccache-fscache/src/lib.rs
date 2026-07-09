//! File metadata cache for zccache.
//!
//! Provides a fast, concurrent, in-memory cache of filesystem metadata
//! to reduce redundant stat calls during compilation.

pub mod cache_system;
pub mod clock;
pub mod metadata;
pub mod persistence;
pub mod verify;

pub use cache_system::CacheSystem;
pub use clock::{ChangeJournal, Clock};
pub use metadata::{Confidence, FileMetadata, MetadataCache};
pub use persistence::FORMAT_VERSION;
pub use verify::VerifyResult;

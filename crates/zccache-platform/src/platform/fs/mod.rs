//! Neutral filesystem mechanics: file/volume identity, same-file
//! comparison, change markers, hard-link counts, permissions, atomic
//! replace, symlink/reparse classification, path-key normalization,
//! clone/reflink/positioned I/O, and free-space probes.
//!
//! Policy — cache layout, transaction ordering, materialization tiers,
//! mtime handling, retry budgets, and authorization to delete — stays with
//! the callers (zccache#1367 moves primitives, not policy).

pub mod durability;
pub mod identity;
pub mod links;
pub mod path;
pub mod permissions;
pub mod replace;
pub mod volume;

pub use identity::{ChangeMarker, FileIdentity};
pub use links::LinkKind;
pub use volume::VolumeIdentity;

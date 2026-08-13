//! Neutral filesystem mechanics: file/volume identity, same-file
//! comparison, change markers, hard-link counts, permissions, atomic
//! replace, symlink/reparse classification, path-key normalization,
//! clone/reflink/positioned I/O, and free-space probes.
//!
//! Policy — cache layout, transaction ordering, materialization tiers,
//! mtime handling, retry budgets, and authorization to delete — stays with
//! the callers. Populated in the filesystem phase (#1367); empty index until
//! then.

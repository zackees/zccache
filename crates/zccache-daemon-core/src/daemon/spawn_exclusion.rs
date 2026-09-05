//! Mutual exclusion between staged-output materialization and child spawn
//! (zackees/zccache#1562, zackees/soldr#3098).
//!
//! # The race
//!
//! A staged MISS is materialized by copying the private compiler output into a
//! unique sibling temporary beside the requested path and then renaming that
//! temporary over the requested path. The copy holds a write descriptor on the
//! temporary's inode. The same daemon process spawns compiler children
//! continuously, and on POSIX every `fork` duplicates the parent's descriptor
//! table: a child forked while the copy's descriptor is open inherits that
//! write descriptor and keeps it until its own `execve` closes it
//! (`O_CLOEXEC` closes on exec, not on fork).
//!
//! `rename(2)` does not change the inode, so after the daemon closes its own
//! descriptor and publishes the path the inherited copy is still a *writable
//! descriptor on the published inode*. Cargo hard-links `build-script-build`
//! to that inode and `execve`s it; `ETXTBSY` is evaluated against the inode,
//! so the exec fails with `Text file busy` for exactly the child's
//! fork-to-exec window. Inspecting the daemon's own `/proc/self/fd` after the
//! rename cannot see the descriptor: it lives in the child's table.
//!
//! # The fix
//!
//! A process-wide `RwLock<()>`. Every child spawn holds the shared guard for
//! the duration of the spawn call (the parent returns from `spawn` only after
//! the child has exec'd or failed, so the shared guard brackets the entire
//! fork-to-exec window). Materialization holds the exclusive guard from the
//! moment the sibling temporary is opened for writing until the rename has
//! published it, so no child can be forked while a materialization descriptor
//! is open, and no descriptor can be opened while a child is between fork and
//! exec.
//!
//! The critical sections contain no `.await`: the guard is a `std` lock held
//! across a synchronous spawn or a synchronous copy + rename only.

use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

static SPAWN_MATERIALIZE_LOCK: RwLock<()> = RwLock::new(());

/// Shared guard for one child spawn. Hold it for the spawn call only.
pub(crate) fn spawn_shared() -> RwLockReadGuard<'static, ()> {
    SPAWN_MATERIALIZE_LOCK
        .read()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Exclusive guard for one materialization copy + publish. Hold it from before
/// the sibling temporary is opened for writing until after the rename.
pub(crate) fn materialize_exclusive() -> RwLockWriteGuard<'static, ()> {
    SPAWN_MATERIALIZE_LOCK
        .write()
        .unwrap_or_else(PoisonError::into_inner)
}

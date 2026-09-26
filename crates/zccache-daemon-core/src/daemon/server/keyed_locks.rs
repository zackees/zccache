//! Per-key async locks for single-flight work (FastLED/fbuild#1466).
//!
//! A cold daemon receives a burst of compiles for the same compiler at once.
//! Probe caches that check, drop their lock, probe, then insert let every
//! request in that burst miss and spawn its own probe. Holding the key's
//! lock across "re-check, probe, insert" makes the first request probe while
//! the rest wait and then read its result. Weak values let idle keys drop.

use super::*;

#[derive(Default)]
pub(super) struct KeyedLocks {
    locks: DashMap<NormalizedPath, std::sync::Weak<Mutex<()>>>,
}

impl KeyedLocks {
    /// The lock for `key`. The returned `Arc` is independent of the map's
    /// entry guard, so callers can await `lock_owned` without holding a
    /// DashMap shard lock.
    pub(super) fn get(&self, key: &NormalizedPath) -> Arc<Mutex<()>> {
        match self.locks.entry(key.clone()) {
            dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                if let Some(lock) = entry.get().upgrade() {
                    lock
                } else {
                    let lock = Arc::new(Mutex::new(()));
                    entry.insert(Arc::downgrade(&lock));
                    lock
                }
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                let lock = Arc::new(Mutex::new(()));
                entry.insert(Arc::downgrade(&lock));
                lock
            }
        }
    }
}

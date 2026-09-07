//! Concrete file watcher backed by the shared systems facade.
//!
//! Creates a `kernal_api::platform::fs_watch::Watcher` that converts host
//! filesystem notifications into `WatchEvent`s, filters them through an
//! `IgnoreFilter`, and sends them over a `tokio::sync::mpsc` channel for
//! consumption by the settle buffer.
//!
//! The watcher callback runs on a dedicated OS thread owned by the facade's
//! backend. Using `tokio::sync::mpsc` (not crossbeam) ensures safe crossing
//! from the OS thread into the async runtime.

use super::ignore::IgnoreFilter;
use super::WatchEvent;
use kernal_api::platform::fs_watch::{
    ChangeEvent, ChangeKind, RecursiveMode, RenameSide, WatchNotification, Watcher,
};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;

/// File watcher backed by the shared systems facade.
///
/// Wraps the facade watcher and exposes `watch`/`unwatch` methods.
/// Events are sent to the unbounded receiver returned by [`NotifyWatcher::new`].
pub struct NotifyWatcher {
    watcher: Watcher,
}

impl std::fmt::Debug for NotifyWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotifyWatcher").finish_non_exhaustive()
    }
}

impl NotifyWatcher {
    /// Create a new watcher with the given ignore filter.
    ///
    /// Returns the watcher and an unbounded receiver of `WatchEvent`s.
    /// The receiver should be fed into a [`SettleBuffer`](super::settle::SettleBuffer).
    ///
    /// # Errors
    ///
    /// Returns an error if the OS file watcher cannot be initialized.
    pub fn new(
        ignore: Arc<IgnoreFilter>,
    ) -> zccache_core::Result<(Self, mpsc::UnboundedReceiver<WatchEvent>)> {
        let (tx, rx) = mpsc::unbounded_channel();

        let watcher = Watcher::new(move |result: std::io::Result<WatchNotification>| {
            match result {
                // kernal-api#76: an incomplete view is a notification, not an
                // error. Both Windows `ReadDirectoryChangesW` conditions --
                // a filled buffer and a watch that silently died -- used to
                // arrive here as `Err`, and an adapter that only matched
                // `Err` would stop seeing them entirely. `watch_lost` also
                // means the watch itself is gone, so the caller must re-watch
                // rather than merely rescan.
                Ok(notification) => {
                    if let WatchNotification::RescanRequired(rescan) = &notification {
                        if rescan.watch_lost() {
                            tracing::warn!(
                                paths = ?rescan.paths(),
                                "watch was lost; paths must be re-watched after the rescan"
                            );
                        }
                    }
                    for watch_event in convert_notification(&ignore, &notification) {
                        if tx.send(watch_event).is_err() {
                            // Receiver dropped — watcher is shutting down.
                            return;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("watcher error: {e}");
                    let _ = tx.send(WatchEvent::Error(e.to_string()));
                }
            }
        })?;

        Ok((Self { watcher }, rx))
    }

    /// Start watching a single directory (non-recursive).
    ///
    /// Callers are responsible for enumerating subdirectories and watching
    /// each one individually. This avoids platform-level recursive watches
    /// that can hit OS limits or produce degenerate behaviour on large trees.
    ///
    /// # Errors
    ///
    /// Returns an error if the path cannot be watched.
    pub fn watch(&mut self, path: &Path) -> zccache_core::Result<()> {
        self.watcher.watch(path, RecursiveMode::NonRecursive)?;
        Ok(())
    }

    /// Start watching a directory recursively.
    ///
    /// This is intended for library consumers that want a single root watch.
    ///
    /// # Errors
    ///
    /// Returns an error if the path cannot be watched.
    pub fn watch_recursive(&mut self, path: &Path) -> zccache_core::Result<()> {
        self.watcher.watch(path, RecursiveMode::Recursive)?;
        Ok(())
    }

    /// Stop watching a directory.
    ///
    /// # Errors
    ///
    /// Returns an error if the path was not being watched.
    pub fn unwatch(&mut self, path: &Path) -> zccache_core::Result<()> {
        self.watcher.unwatch(path)?;
        Ok(())
    }
}

/// Convert a facade notification into zero or more `WatchEvent`s.
///
/// Split out from the watcher callback so the rescan routing is testable
/// without standing up a real watcher and racing a real filesystem.
fn convert_notification(
    ignore: &IgnoreFilter,
    notification: &WatchNotification,
) -> Vec<WatchEvent> {
    match notification {
        // Both halves are overflow to this crate: the view is incomplete
        // either way, and every watched path must be treated as stale. The
        // `watch_lost` half additionally needs re-watching, which the caller
        // handles because only it holds the watcher.
        WatchNotification::RescanRequired(_) => vec![WatchEvent::Overflow],
        WatchNotification::Change(event) => convert_event(ignore, event),
    }
}

/// Convert a facade [`ChangeEvent`] into zero or more `WatchEvent`s.
///
/// Overflow is deliberately not handled here any more. The facade reports an
/// incomplete view as its own `WatchNotification::RescanRequired` variant
/// rather than as an event with a rescan flag and no paths, so it is matched
/// where notifications arrive. That also removes the failure mode the old
/// comment warned about: there is no longer an event whose path loop yields
/// nothing and silently swallows the overflow.
fn convert_event(ignore: &IgnoreFilter, event: &ChangeEvent) -> Vec<WatchEvent> {
    let paths = event.paths();

    // Handle rename with both paths present.
    if matches!(event.kind(), ChangeKind::NameModified(RenameSide::Both)) && paths.len() >= 2 {
        let from = &paths[0];
        let to = &paths[1];
        let from_ignored = ignore.should_ignore(from);
        let to_ignored = ignore.should_ignore(to);
        if from_ignored && to_ignored {
            return vec![];
        }
        if from_ignored {
            // File appeared from ignored area — treat as creation.
            return vec![WatchEvent::Created(to.as_path().into())];
        }
        if to_ignored {
            // File moved to ignored area — treat as removal.
            return vec![WatchEvent::Removed(from.as_path().into())];
        }
        return vec![WatchEvent::Renamed {
            from: from.as_path().into(),
            to: to.as_path().into(),
        }];
    }

    let mut result = Vec::new();
    for path in paths {
        if ignore.should_ignore(path) {
            continue;
        }

        let watch_event = match event.kind() {
            ChangeKind::Created(_) => WatchEvent::Created(path.as_path().into()),
            ChangeKind::Removed(_) => WatchEvent::Removed(path.as_path().into()),
            ChangeKind::NameModified(RenameSide::From) => {
                // Half of a rename — treat as removal (conservative).
                WatchEvent::Removed(path.as_path().into())
            }
            ChangeKind::NameModified(RenameSide::To) => {
                // Half of a rename — treat as creation (conservative).
                WatchEvent::Created(path.as_path().into())
            }
            ChangeKind::ContentModified
            | ChangeKind::MetadataModified
            // A `Both` rename with fewer than two paths never reached the
            // branch above, and `Unknown` is a rename whose side the host did
            // not say. Both are modifications, conservatively.
            | ChangeKind::NameModified(_) => WatchEvent::Modified(path.as_path().into()),
            ChangeKind::Accessed => continue,
            ChangeKind::Other => WatchEvent::Modified(path.as_path().into()),
        };

        result.push(watch_event);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the fixtures name these: the conversion above reads a kind, it
    // does not build one.
    use kernal_api::platform::fs_watch::{EntryKind, RescanRequired};

    fn test_filter() -> IgnoreFilter {
        IgnoreFilter::new(vec![".git".to_string(), "target".to_string()])
    }

    #[test]
    fn convert_create_event() {
        let filter = test_filter();
        let event = ChangeEvent::new(
            ChangeKind::Created(EntryKind::File),
            vec![Path::new("src/main.rs").to_owned()],
        );
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(
            matches!(&result[0], WatchEvent::Created(p) if p.as_path() == Path::new("src/main.rs"))
        );
    }

    #[test]
    fn convert_modify_event() {
        let filter = test_filter();
        let event = ChangeEvent::new(
            ChangeKind::ContentModified,
            vec![Path::new("src/lib.rs").to_owned()],
        );
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(
            matches!(&result[0], WatchEvent::Modified(p) if p.as_path() == Path::new("src/lib.rs"))
        );
    }

    #[test]
    fn convert_remove_event() {
        let filter = test_filter();
        let event = ChangeEvent::new(
            ChangeKind::Removed(EntryKind::File),
            vec![Path::new("old.c").to_owned()],
        );
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Removed(p) if p.as_path() == Path::new("old.c")));
    }

    #[test]
    fn convert_rename_both() {
        let filter = test_filter();
        let event = ChangeEvent::new(
            ChangeKind::NameModified(RenameSide::Both),
            vec![Path::new("old.c").to_owned(), Path::new("new.c").to_owned()],
        );
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(
            &result[0],
            WatchEvent::Renamed { from, to }
            if from.as_path() == Path::new("old.c") && to.as_path() == Path::new("new.c")
        ));
    }

    #[test]
    fn convert_rename_from_becomes_removed() {
        let filter = test_filter();
        let event = ChangeEvent::new(
            ChangeKind::NameModified(RenameSide::From),
            vec![Path::new("gone.c").to_owned()],
        );
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Removed(p) if p.as_path() == Path::new("gone.c")));
    }

    #[test]
    fn convert_rename_to_becomes_created() {
        let filter = test_filter();
        let event = ChangeEvent::new(
            ChangeKind::NameModified(RenameSide::To),
            vec![Path::new("appeared.c").to_owned()],
        );
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(
            matches!(&result[0], WatchEvent::Created(p) if p.as_path() == Path::new("appeared.c"))
        );
    }

    #[test]
    fn ignored_paths_filtered_out() {
        let filter = test_filter();
        let event = ChangeEvent::new(
            ChangeKind::ContentModified,
            vec![Path::new("project/.git/index").to_owned()],
        );
        let result = convert_event(&filter, &event);
        assert!(result.is_empty());
    }

    #[test]
    fn ignored_rename_both_filtered() {
        let filter = test_filter();
        let event = ChangeEvent::new(
            ChangeKind::NameModified(RenameSide::Both),
            vec![
                Path::new("project/.git/old").to_owned(),
                Path::new("project/.git/new").to_owned(),
            ],
        );
        let result = convert_event(&filter, &event);
        assert!(result.is_empty());
    }

    #[test]
    fn rename_from_ignored_to_visible_becomes_created() {
        // Rename from an ignored dir to a visible dir should produce Created(to).
        let filter = test_filter();
        let event = ChangeEvent::new(
            ChangeKind::NameModified(RenameSide::Both),
            vec![
                Path::new("project/.git/stash").to_owned(),
                Path::new("src/recovered.c").to_owned(),
            ],
        );
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(
            matches!(&result[0], WatchEvent::Created(p) if p.as_path() == Path::new("src/recovered.c"))
        );
    }

    #[test]
    fn rename_from_visible_to_ignored_becomes_removed() {
        // Rename from a visible dir to an ignored dir should produce Removed(from).
        let filter = test_filter();
        let event = ChangeEvent::new(
            ChangeKind::NameModified(RenameSide::Both),
            vec![
                Path::new("src/main.rs").to_owned(),
                Path::new("project/.git/stash").to_owned(),
            ],
        );
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(
            matches!(&result[0], WatchEvent::Removed(p) if p.as_path() == Path::new("src/main.rs"))
        );
    }

    #[test]
    fn access_events_ignored() {
        let filter = test_filter();
        let event = ChangeEvent::new(
            ChangeKind::Accessed,
            vec![Path::new("src/main.rs").to_owned()],
        );
        let result = convert_event(&filter, &event);
        assert!(result.is_empty());
    }

    #[test]
    fn rename_both_with_single_path_falls_through() {
        // Rename Both with < 2 paths should not panic; falls to per-path loop.
        let filter = test_filter();
        let event = ChangeEvent::new(
            ChangeKind::NameModified(RenameSide::Both),
            vec![Path::new("only_one.c").to_owned()],
        );
        let result = convert_event(&filter, &event);
        // Falls through to per-path handling as Modify(Name(Both)), caught by wildcard → Modified.
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Modified(_)));
    }

    #[test]
    fn event_with_empty_paths() {
        let filter = test_filter();
        let event = ChangeEvent::new(ChangeKind::ContentModified, vec![]);
        let result = convert_event(&filter, &event);
        assert!(result.is_empty());
    }

    #[test]
    fn event_kind_other_becomes_modified() {
        let filter = test_filter();
        let event = ChangeEvent::new(ChangeKind::Other, vec![Path::new("mystery.c").to_owned()]);
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Modified(_)));
    }

    #[test]
    fn event_kind_any_becomes_modified() {
        let filter = test_filter();
        let event = ChangeEvent::new(ChangeKind::Other, vec![Path::new("any.c").to_owned()]);
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Modified(_)));
    }

    #[test]
    fn remove_directory_event() {
        let filter = test_filter();
        let event = ChangeEvent::new(
            ChangeKind::Removed(EntryKind::Folder),
            vec![Path::new("src/old_module").to_owned()],
        );
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Removed(_)));
    }

    #[test]
    fn create_directory_event() {
        let filter = test_filter();
        let event = ChangeEvent::new(
            ChangeKind::Created(EntryKind::Folder),
            vec![Path::new("src/new_module").to_owned()],
        );
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Created(_)));
    }

    #[test]
    fn metadata_change_becomes_modified() {
        let filter = test_filter();
        let event = ChangeEvent::new(
            ChangeKind::MetadataModified,
            vec![Path::new("script.sh").to_owned()],
        );
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Modified(_)));
    }

    #[test]
    fn notify_watcher_can_be_created() {
        use std::sync::Arc;

        let ignore = Arc::new(IgnoreFilter::default());
        let result = NotifyWatcher::new(ignore);
        assert!(result.is_ok());

        let (mut watcher, _rx) = result.unwrap();
        // Watch a valid temp dir.
        let dir = tempfile::TempDir::new().unwrap();
        assert!(watcher.watch(dir.path()).is_ok());
        assert!(watcher.unwatch(dir.path()).is_ok());
    }

    #[test]
    fn notify_watcher_watch_nonexistent_fails() {
        use std::sync::Arc;

        let ignore = Arc::new(IgnoreFilter::default());
        let (mut watcher, _rx) = NotifyWatcher::new(ignore).unwrap();
        let result = watcher.watch(Path::new("/no/such/directory/ever"));
        assert!(result.is_err());
    }

    #[test]
    fn notify_watcher_debug_impl() {
        use std::sync::Arc;
        let ignore = Arc::new(IgnoreFilter::default());
        let (watcher, _rx) = NotifyWatcher::new(ignore).unwrap();
        let debug = format!("{watcher:?}");
        assert!(debug.contains("NotifyWatcher"));
    }

    #[test]
    fn a_rescan_produces_overflow() {
        let filter = test_filter();
        // Was an `EventKind::Other` carrying `Flag::Rescan`; the facade
        // reports it as its own variant (kernal-api#76), so this is now a
        // notification rather than an event with a flag.
        let notification =
            WatchNotification::RescanRequired(RescanRequired::new(false, Vec::new()));
        let result = convert_notification(&filter, &notification);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Overflow));
    }

    #[test]
    fn a_rescan_with_paths_still_produces_overflow() {
        let filter = test_filter();
        // Even when a rescan names paths, it is overflow: the semantics are
        // "everything may have changed", not "these changed".
        let notification = WatchNotification::RescanRequired(RescanRequired::new(
            false,
            vec![Path::new("src/main.rs").to_owned()],
        ));
        let result = convert_notification(&filter, &notification);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Overflow));
    }

    /// A lost watch is still overflow to this crate, and must not be dropped
    /// merely because it also means the watch has to be re-established.
    /// kernal-api#76's migration note is explicit that an adapter matching
    /// only on `Err` stops seeing both Windows conditions entirely.
    #[test]
    fn a_lost_watch_is_reported_as_overflow_too() {
        let filter = test_filter();
        let notification = WatchNotification::RescanRequired(RescanRequired::new(
            true,
            vec![Path::new("src").to_owned()],
        ));
        let result = convert_notification(&filter, &notification);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Overflow));
    }

    #[test]
    fn mixed_paths_filter_individually() {
        let filter = test_filter();
        let event = ChangeEvent::new(
            ChangeKind::ContentModified,
            vec![
                Path::new("src/main.rs").to_owned(),
                Path::new("target/debug/binary").to_owned(),
                Path::new("src/lib.rs").to_owned(),
            ],
        );
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 2);
    }
}

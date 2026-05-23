//! Concrete file watcher backed by the `notify` crate.
//!
//! Creates a `RecommendedWatcher` that converts OS filesystem events into
//! `WatchEvent`s, filters them through an `IgnoreFilter`, and sends them
//! over a `tokio::sync::mpsc` channel for consumption by the settle buffer.
//!
//! The notify callback runs on a dedicated OS thread. Using
//! `tokio::sync::mpsc` (not crossbeam) ensures safe crossing from the OS
//! thread into the async runtime.

use super::ignore::IgnoreFilter;
use super::WatchEvent;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;

/// File watcher backed by the `notify` crate.
///
/// Wraps a `RecommendedWatcher` and exposes `watch`/`unwatch` methods.
/// Events are sent to the unbounded receiver returned by [`NotifyWatcher::new`].
pub struct NotifyWatcher {
    watcher: notify::RecommendedWatcher,
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
    ) -> crate::core::Result<(Self, mpsc::UnboundedReceiver<WatchEvent>)> {
        let (tx, rx) = mpsc::unbounded_channel();

        let watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            match res {
                Ok(event) => {
                    for watch_event in convert_event(&ignore, &event) {
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
        })
        .map_err(|e| std::io::Error::other(e.to_string()))?;

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
    pub fn watch(&mut self, path: &Path) -> crate::core::Result<()> {
        self.watcher
            .watch(path, RecursiveMode::NonRecursive)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(())
    }

    /// Start watching a directory recursively.
    ///
    /// This is intended for library consumers that want a single root watch.
    ///
    /// # Errors
    ///
    /// Returns an error if the path cannot be watched.
    pub fn watch_recursive(&mut self, path: &Path) -> crate::core::Result<()> {
        self.watcher
            .watch(path, RecursiveMode::Recursive)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(())
    }

    /// Stop watching a directory.
    ///
    /// # Errors
    ///
    /// Returns an error if the path was not being watched.
    pub fn unwatch(&mut self, path: &Path) -> crate::core::Result<()> {
        self.watcher
            .unwatch(path)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(())
    }
}

/// Convert a `notify::Event` into zero or more `WatchEvent`s.
fn convert_event(ignore: &IgnoreFilter, event: &Event) -> Vec<WatchEvent> {
    // Detect overflow/rescan events from inotify (Q_OVERFLOW) and
    // FSEvents (MUST_SCAN_SUBDIRS). These have EventKind::Other with
    // Flag::Rescan and empty paths — the path loop below would produce
    // nothing, silently swallowing the overflow.
    if event.need_rescan() {
        return vec![WatchEvent::Overflow];
    }

    // Handle rename with both paths present.
    if matches!(
        event.kind,
        EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::Both
        ))
    ) && event.paths.len() >= 2
    {
        let from = &event.paths[0];
        let to = &event.paths[1];
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
    for path in &event.paths {
        if ignore.should_ignore(path) {
            continue;
        }

        let watch_event = match event.kind {
            EventKind::Create(_) => WatchEvent::Created(path.as_path().into()),
            EventKind::Remove(_) => WatchEvent::Removed(path.as_path().into()),
            EventKind::Modify(notify::event::ModifyKind::Name(notify::event::RenameMode::From)) => {
                // Half of a rename — treat as removal (conservative).
                WatchEvent::Removed(path.as_path().into())
            }
            EventKind::Modify(notify::event::ModifyKind::Name(notify::event::RenameMode::To)) => {
                // Half of a rename — treat as creation (conservative).
                WatchEvent::Created(path.as_path().into())
            }
            EventKind::Modify(_) => WatchEvent::Modified(path.as_path().into()),
            EventKind::Access(_) => continue,
            // Any, Other — conservative: treat as modification.
            _ => WatchEvent::Modified(path.as_path().into()),
        };

        result.push(watch_event);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_filter() -> IgnoreFilter {
        IgnoreFilter::new(vec![".git".to_string(), "target".to_string()])
    }

    #[test]
    fn convert_create_event() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Create(notify::event::CreateKind::File),
            paths: vec![Path::new("src/main.rs").to_owned()],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(
            matches!(&result[0], WatchEvent::Created(p) if p.as_path() == Path::new("src/main.rs"))
        );
    }

    #[test]
    fn convert_modify_event() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            paths: vec![Path::new("src/lib.rs").to_owned()],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(
            matches!(&result[0], WatchEvent::Modified(p) if p.as_path() == Path::new("src/lib.rs"))
        );
    }

    #[test]
    fn convert_remove_event() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Remove(notify::event::RemoveKind::File),
            paths: vec![Path::new("old.c").to_owned()],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Removed(p) if p.as_path() == Path::new("old.c")));
    }

    #[test]
    fn convert_rename_both() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::Both,
            )),
            paths: vec![Path::new("old.c").to_owned(), Path::new("new.c").to_owned()],
            attrs: Default::default(),
        };
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
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::From,
            )),
            paths: vec![Path::new("gone.c").to_owned()],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Removed(p) if p.as_path() == Path::new("gone.c")));
    }

    #[test]
    fn convert_rename_to_becomes_created() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::To,
            )),
            paths: vec![Path::new("appeared.c").to_owned()],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(
            matches!(&result[0], WatchEvent::Created(p) if p.as_path() == Path::new("appeared.c"))
        );
    }

    #[test]
    fn ignored_paths_filtered_out() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            paths: vec![Path::new("project/.git/index").to_owned()],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert!(result.is_empty());
    }

    #[test]
    fn ignored_rename_both_filtered() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::Both,
            )),
            paths: vec![
                Path::new("project/.git/old").to_owned(),
                Path::new("project/.git/new").to_owned(),
            ],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert!(result.is_empty());
    }

    #[test]
    fn rename_from_ignored_to_visible_becomes_created() {
        // Rename from an ignored dir to a visible dir should produce Created(to).
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::Both,
            )),
            paths: vec![
                Path::new("project/.git/stash").to_owned(),
                Path::new("src/recovered.c").to_owned(),
            ],
            attrs: Default::default(),
        };
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
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::Both,
            )),
            paths: vec![
                Path::new("src/main.rs").to_owned(),
                Path::new("project/.git/stash").to_owned(),
            ],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(
            matches!(&result[0], WatchEvent::Removed(p) if p.as_path() == Path::new("src/main.rs"))
        );
    }

    #[test]
    fn access_events_ignored() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Access(notify::event::AccessKind::Read),
            paths: vec![Path::new("src/main.rs").to_owned()],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert!(result.is_empty());
    }

    #[test]
    fn rename_both_with_single_path_falls_through() {
        // Rename Both with < 2 paths should not panic; falls to per-path loop.
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::Both,
            )),
            paths: vec![Path::new("only_one.c").to_owned()],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        // Falls through to per-path handling as Modify(Name(Both)), caught by wildcard → Modified.
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Modified(_)));
    }

    #[test]
    fn event_with_empty_paths() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            paths: vec![],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert!(result.is_empty());
    }

    #[test]
    fn event_kind_other_becomes_modified() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Other,
            paths: vec![Path::new("mystery.c").to_owned()],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Modified(_)));
    }

    #[test]
    fn event_kind_any_becomes_modified() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Any,
            paths: vec![Path::new("any.c").to_owned()],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Modified(_)));
    }

    #[test]
    fn remove_directory_event() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Remove(notify::event::RemoveKind::Folder),
            paths: vec![Path::new("src/old_module").to_owned()],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Removed(_)));
    }

    #[test]
    fn create_directory_event() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Create(notify::event::CreateKind::Folder),
            paths: vec![Path::new("src/new_module").to_owned()],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Created(_)));
    }

    #[test]
    fn metadata_change_becomes_modified() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Metadata(
                notify::event::MetadataKind::Permissions,
            )),
            paths: vec![Path::new("script.sh").to_owned()],
            attrs: Default::default(),
        };
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
    fn rescan_flag_produces_overflow() {
        use notify::event::Flag;

        let filter = test_filter();
        // inotify Q_OVERFLOW and FSEvents MUST_SCAN_SUBDIRS produce
        // EventKind::Other with Flag::Rescan and empty paths.
        let event = Event::new(EventKind::Other).set_flag(Flag::Rescan);
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Overflow));
    }

    #[test]
    fn rescan_flag_with_paths_still_produces_overflow() {
        use notify::event::Flag;

        let filter = test_filter();
        // Even if a rescan event carries paths, we still treat it as overflow
        // because the semantics are "everything may have changed".
        let mut event = Event::new(EventKind::Other).set_flag(Flag::Rescan);
        event.paths = vec![Path::new("src/main.rs").to_owned()];
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Overflow));
    }

    #[test]
    fn mixed_paths_filter_individually() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            paths: vec![
                Path::new("src/main.rs").to_owned(),
                Path::new("target/debug/binary").to_owned(),
                Path::new("src/lib.rs").to_owned(),
            ],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 2);
    }
}

# zccache-watcher

Cross-platform file watching for zccache.

This crate currently has two layers:

- a public Rust crate API used by the daemon watcher pipeline
- an optional `python` feature that exposes a Rust-backed polling engine to `zccache.watcher`

Those layers solve different problems. The Rust crate API is built around the
daemon's `notify`-driven event pipeline and also exposes a public polling
watcher for library-style use. The Python package binds to that Rust polling
watcher and delivers events to Python through polling or callbacks.

## Rust API

The public Rust surface is intended for daemon and systems integration code.

Available types:

- `PollingWatcherConfig`
- `PollingWatcher`
- `PollWatchBatch`
- `PollWatchObserver`
- `IgnoreFilter`
- `NotifyWatcher`
- `SettleBuffer`
- `SettledEvent`
- `OverflowRecovery`
- `WatchEvent`
- `WatcherConfig`

Polling watcher flow:

1. Build a `PollingWatcherConfig`
2. Create a `PollingWatcher`
3. Call `start()`
4. Consume batches with `poll()` / `poll_timeout()`
5. Optionally register observers with `add_observer()` or `add_callback()`
6. Call `stop()` or `resume()` as needed

```rust
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use zccache::watcher::{PollWatchBatch, PollingWatcher, PollingWatcherConfig};

let mut config = PollingWatcherConfig::new(".");
config.include_globs = vec!["**/*.rs".to_string()];
config.excluded_patterns = vec!["target".to_string()];
config.poll_interval = Duration::from_millis(50);
config.debounce = Duration::from_millis(50);

let watcher = PollingWatcher::new(config)?;
let seen = Arc::new(AtomicUsize::new(0));
let seen_clone = Arc::clone(&seen);
watcher.add_callback(move |_batch: &PollWatchBatch| {
    seen_clone.fetch_add(1, Ordering::SeqCst);
})?;
watcher.start()?;
let batch = watcher.poll_timeout(Duration::from_secs(1))?;
watcher.stop()?;
# let _ = batch;
```

Daemon pipeline flow:

1. Create an `IgnoreFilter`
2. Create a `NotifyWatcher`
3. Register directories with `watch()` or `watch_recursive()`
4. Feed the returned raw event receiver into `SettleBuffer::run()`
5. Consume `SettledEvent::Batch` / `SettledEvent::Overflow`

```rust
use std::sync::Arc;
use tokio::sync::mpsc;
use zccache::watcher::{IgnoreFilter, NotifyWatcher, SettleBuffer, SettledEvent};

async fn run_watcher(root: &std::path::Path) -> zccache::core::Result<()> {
    let ignore = Arc::new(IgnoreFilter::default());
    let (mut watcher, raw_rx) = NotifyWatcher::new(ignore)?;
    watcher.watch_recursive(root)?;

    let settle = SettleBuffer::default_window();
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        settle.run(raw_rx, tx).await;
    });

    while let Some(event) = rx.recv().await {
        match event {
            SettledEvent::Batch { changed, removed } => {
                println!("changed={changed:?} removed={removed:?}");
            }
            SettledEvent::Overflow => {
                println!("overflow");
            }
        }
    }
    Ok(())
}
```

Notes:

- `PollingWatcher` is the public Rust API closest to the Python watcher surface.
- `resume()` resets the baseline, matching the Python watcher lifecycle semantics.
- `NotifyWatcher` remains the lower-level public Rust entrypoint for the daemon pipeline.
- `WatcherConfig` currently carries settle-window and ignore-pattern defaults
  used by the daemon-oriented watcher pipeline.

## Python API

The Python package is published as `zccache.watcher`.

It supports:

- polling-style user APIs
- callback-style user APIs
- explicit `start()`, `stop()`, and `resume()` lifecycle control
- context-manager usage
- internal Rust worker polling to avoid OS event queue saturation
- `include_folders` to limit scan scope
- `include_globs` to select files by glob
- `excluded_patterns` to skip files or whole directories
- `notification_predicate` for late-binding Python-side filtering
- `debounce_seconds` to coalesce rapid edits

### Polling API

```python
from zccache.watcher import watch_files

watcher = watch_files(
    ".",
    include_folders=["src", "include"],
    include_globs=["src/**/*.cpp", "include/**/*.h"],
    excluded_patterns=["build", "dist/**", ".git", "__pycache__"],
    debounce_seconds=0.2,
    poll_interval=0.1,
)

event = watcher.poll(timeout=1.0)
if event is not None:
    print(event.paths)

watcher.stop()
```

### Late-Binding Predicate Filter

```python
from pathlib import Path
from zccache.watcher import FileWatcher

def keep_notification(
    path: Path,
    *,
    relative_path: str,
    change: str,
    root: Path,
) -> bool:
    return not relative_path.endswith(".tmp")

watcher = FileWatcher(
    ".",
    include_globs=["**/*.cpp", "**/*.h"],
    notification_predicate=keep_notification,
)
```

The predicate runs after the internal scan has detected a pending change but
before the event is delivered to `poll()` or callbacks. Return `True` to keep
the notification and `False` to suppress it.

### Callback API

```python
from zccache.watcher import watch_files

def on_change(event):
    print("changed:", event.changed)
    print("removed:", event.removed)

watcher = watch_files(
    ".",
    include_globs=["**/*.py"],
    excluded_patterns=[".venv", "__pycache__"],
    callback=on_change,
)
```

### Lifecycle-Controlled Class API

```python
from zccache.watcher import FileWatcher

watcher = FileWatcher(
    ".",
    include_globs=["**/*.cpp"],
    excluded_patterns=["build", ".git"],
    autostart=False,
)

with watcher:
    event = watcher.poll(timeout=1.0)
    if event is not None:
        print(event.paths)

watcher.resume()
watcher.stop()
```

### KeyboardInterrupt Handling

The Python wrapper logs `KeyboardInterrupt: watcher stopped` once per watcher
instance when delivery is interrupted. Interrupt propagation is thread-aware:

- on the main thread, the original `KeyboardInterrupt` is re-raised
- on a worker thread, the wrapper notifies the main thread with `_thread.interrupt_main()`

### fastled-wasm Compatibility

The package keeps the compatibility names used by `fastled-wasm`:

- `FileWatcherProcess`
- `DebouncedFileWatcherProcess`
- `file_watcher_enabled()`
- `file_watcher_set()`

`FileWatcherProcess.get_all_changes()` remains the simplest drop-in polling API.

## CLI Relationship

If you only need a "did anything relevant change?" answer instead of a file
event stream, use the main `zccache` CLI fingerprint API:

```bash
zccache fp --cache-file .cache/inputs.json check \
  --root . \
  --include '**/*.rs' \
  --exclude target
```

Related commands:

- `zccache fp --cache-file .cache/inputs.json mark-success`
- `zccache fp --cache-file .cache/inputs.json mark-failure`
- `zccache fp --cache-file .cache/inputs.json invalidate`

That CLI path is daemon-backed and optimized for build-step invalidation rather
than event delivery.

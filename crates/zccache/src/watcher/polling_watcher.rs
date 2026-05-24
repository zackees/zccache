use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::core::NormalizedPath;
use globset::{Glob, GlobSet, GlobSetBuilder};

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileState {
    mtime_ns: u128,
    size: u64,
}

#[derive(Clone)]
struct ScanConfig {
    root: NormalizedPath,
    include_folders: Vec<NormalizedPath>,
    include_globs: GlobSet,
    exclude_globs: GlobSet,
    excluded_names: HashSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollWatchBatch {
    pub changed: Vec<NormalizedPath>,
    pub removed: Vec<NormalizedPath>,
    pub overflow: bool,
}

impl PollWatchBatch {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.removed.is_empty() && !self.overflow
    }
}

pub trait PollWatchObserver: Send + Sync {
    fn on_batch(&self, batch: &PollWatchBatch);
}

struct FnObserver<F> {
    callback: F,
}

impl<F> PollWatchObserver for FnObserver<F>
where
    F: Fn(&PollWatchBatch) + Send + Sync + 'static,
{
    fn on_batch(&self, batch: &PollWatchBatch) {
        (self.callback)(batch);
    }
}

#[derive(Clone, Debug)]
pub struct PollingWatcherConfig {
    pub root: NormalizedPath,
    pub include_folders: Vec<NormalizedPath>,
    pub include_globs: Vec<String>,
    pub excluded_patterns: Vec<String>,
    pub poll_interval: Duration,
    pub debounce: Duration,
}

impl PollingWatcherConfig {
    #[must_use]
    pub fn new(root: impl Into<NormalizedPath>) -> Self {
        Self {
            root: root.into(),
            include_folders: Vec::new(),
            include_globs: Vec::new(),
            excluded_patterns: Vec::new(),
            poll_interval: Duration::from_millis(100),
            debounce: Duration::from_millis(200),
        }
    }
}

pub struct PollingWatcher {
    config: ScanConfig,
    poll_interval: Duration,
    debounce: Duration,
    observers: Arc<Mutex<Vec<Arc<dyn PollWatchObserver>>>>,
    poll_rx: Mutex<Option<mpsc::Receiver<PollWatchBatch>>>,
    worker_shutdown_tx: Mutex<Option<mpsc::Sender<()>>>,
    worker_handle: Mutex<Option<JoinHandle<()>>>,
    dispatch_shutdown_tx: Mutex<Option<mpsc::Sender<()>>>,
    dispatch_handle: Mutex<Option<JoinHandle<()>>>,
}

impl PollingWatcher {
    pub fn new(config: PollingWatcherConfig) -> std::io::Result<Self> {
        let root = config.root;
        if !root.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "watch root does not exist or is not a directory: {}",
                    root.display()
                ),
            ));
        }

        let scan_config = build_config(
            &root,
            &config.include_folders,
            &config.include_globs,
            &config.excluded_patterns,
        )?;

        Ok(Self {
            config: scan_config,
            poll_interval: config.poll_interval.max(Duration::from_millis(1)),
            debounce: config.debounce,
            observers: Arc::new(Mutex::new(Vec::new())),
            poll_rx: Mutex::new(None),
            worker_shutdown_tx: Mutex::new(None),
            worker_handle: Mutex::new(None),
            dispatch_shutdown_tx: Mutex::new(None),
            dispatch_handle: Mutex::new(None),
        })
    }

    pub fn start(&self) -> std::io::Result<()> {
        if self.is_running() {
            return Ok(());
        }

        let (worker_batch_tx, worker_batch_rx) = mpsc::channel();
        let (poll_tx, poll_rx) = mpsc::channel();
        let (worker_shutdown_tx, worker_shutdown_rx) = mpsc::channel();
        let (dispatch_shutdown_tx, dispatch_shutdown_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let config = self.config.clone();
        let poll_interval = self.poll_interval;
        let debounce = self.debounce;
        let observers = Arc::clone(&self.observers);

        let worker_handle = thread::Builder::new()
            .name("zccache-polling-watcher".to_string())
            .spawn(move || {
                run_poll_loop(
                    config,
                    poll_interval,
                    debounce,
                    worker_batch_tx,
                    worker_shutdown_rx,
                    ready_tx,
                )
            })?;

        match ready_rx.recv() {
            Ok(()) => {}
            Err(_) => {
                let _ = worker_handle.join();
                return Err(std::io::Error::other(
                    "watcher worker exited before initialization completed",
                ));
            }
        }

        let dispatch_handle = thread::Builder::new()
            .name("zccache-polling-watcher-dispatch".to_string())
            .spawn(move || {
                run_dispatch_loop(worker_batch_rx, poll_tx, dispatch_shutdown_rx, observers)
            })?;

        *self
            .poll_rx
            .lock()
            .map_err(|_| std::io::Error::other("watcher receiver lock poisoned"))? = Some(poll_rx);
        *self
            .worker_shutdown_tx
            .lock()
            .map_err(|_| std::io::Error::other("watcher shutdown lock poisoned"))? =
            Some(worker_shutdown_tx);
        *self
            .worker_handle
            .lock()
            .map_err(|_| std::io::Error::other("watcher worker lock poisoned"))? =
            Some(worker_handle);
        *self
            .dispatch_shutdown_tx
            .lock()
            .map_err(|_| std::io::Error::other("watcher dispatch shutdown lock poisoned"))? =
            Some(dispatch_shutdown_tx);
        *self
            .dispatch_handle
            .lock()
            .map_err(|_| std::io::Error::other("watcher dispatch lock poisoned"))? =
            Some(dispatch_handle);

        Ok(())
    }

    pub fn resume(&self) -> std::io::Result<()> {
        self.start()
    }

    pub fn stop(&self) -> std::io::Result<()> {
        let worker_shutdown = self
            .worker_shutdown_tx
            .lock()
            .map_err(|_| std::io::Error::other("watcher shutdown lock poisoned"))?
            .take();
        if let Some(tx) = worker_shutdown {
            let _ = tx.send(());
        }

        let dispatch_shutdown = self
            .dispatch_shutdown_tx
            .lock()
            .map_err(|_| std::io::Error::other("watcher dispatch shutdown lock poisoned"))?
            .take();
        if let Some(tx) = dispatch_shutdown {
            let _ = tx.send(());
        }

        let worker = self
            .worker_handle
            .lock()
            .map_err(|_| std::io::Error::other("watcher worker lock poisoned"))?
            .take();
        if let Some(handle) = worker {
            handle
                .join()
                .map_err(|_| std::io::Error::other("watcher worker thread panicked"))?;
        }

        let dispatch = self
            .dispatch_handle
            .lock()
            .map_err(|_| std::io::Error::other("watcher dispatch lock poisoned"))?
            .take();
        if let Some(handle) = dispatch {
            handle
                .join()
                .map_err(|_| std::io::Error::other("watcher dispatch thread panicked"))?;
        }

        *self
            .poll_rx
            .lock()
            .map_err(|_| std::io::Error::other("watcher receiver lock poisoned"))? = None;

        Ok(())
    }

    #[must_use]
    pub fn is_running(&self) -> bool {
        self.worker_handle
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(JoinHandle::is_finished))
            .is_some_and(|finished| !finished)
    }

    pub fn poll(&self) -> std::io::Result<Option<PollWatchBatch>> {
        self.poll_timeout(Duration::ZERO)
    }

    pub fn poll_timeout(&self, timeout: Duration) -> std::io::Result<Option<PollWatchBatch>> {
        let receiver_guard = self
            .poll_rx
            .lock()
            .map_err(|_| std::io::Error::other("watcher receiver lock poisoned"))?;
        let Some(receiver) = receiver_guard.as_ref() else {
            return Ok(None);
        };

        if timeout.is_zero() {
            match receiver.try_recv() {
                Ok(batch) => Ok(Some(batch)),
                Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => Ok(None),
            }
        } else {
            match receiver.recv_timeout(timeout) {
                Ok(batch) => Ok(Some(batch)),
                Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {
                    Ok(None)
                }
            }
        }
    }

    pub fn add_observer(&self, observer: Arc<dyn PollWatchObserver>) -> std::io::Result<()> {
        self.observers
            .lock()
            .map_err(|_| std::io::Error::other("watcher observers lock poisoned"))?
            .push(observer);
        Ok(())
    }

    pub fn add_callback<F>(&self, callback: F) -> std::io::Result<()>
    where
        F: Fn(&PollWatchBatch) + Send + Sync + 'static,
    {
        self.add_observer(Arc::new(FnObserver { callback }))
    }
}

impl Drop for PollingWatcher {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn run_dispatch_loop(
    worker_batch_rx: mpsc::Receiver<PollWatchBatch>,
    poll_tx: mpsc::Sender<PollWatchBatch>,
    dispatch_shutdown_rx: mpsc::Receiver<()>,
    observers: Arc<Mutex<Vec<Arc<dyn PollWatchObserver>>>>,
) {
    loop {
        if dispatch_shutdown_rx.try_recv().is_ok() {
            break;
        }

        let batch = match worker_batch_rx.recv_timeout(Duration::from_millis(25)) {
            Ok(batch) => batch,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        if poll_tx.send(batch.clone()).is_err() {
            break;
        }

        let snapshot = match observers.lock() {
            Ok(guard) => guard.clone(),
            Err(_) => break,
        };
        for observer in snapshot {
            observer.on_batch(&batch);
        }
    }
}

fn run_poll_loop(
    config: ScanConfig,
    poll_interval: Duration,
    debounce: Duration,
    batch_tx: mpsc::Sender<PollWatchBatch>,
    shutdown_rx: mpsc::Receiver<()>,
    ready_tx: mpsc::Sender<()>,
) {
    let mut snapshot = scan_snapshot(&config);
    let _ = ready_tx.send(());
    let mut pending_changed: HashSet<NormalizedPath> = HashSet::new();
    let mut pending_removed: HashSet<NormalizedPath> = HashSet::new();
    let mut last_change: Option<Instant> = None;

    loop {
        if shutdown_rx.try_recv().is_ok() {
            break;
        }

        let current = scan_snapshot(&config);
        let (changed, removed) = diff_snapshots(&snapshot, &current);

        if !changed.is_empty() || !removed.is_empty() {
            for path in changed {
                pending_removed.remove(&path);
                pending_changed.insert(path);
            }
            for path in removed {
                pending_changed.remove(&path);
                pending_removed.insert(path);
            }
            last_change = Some(Instant::now());
        } else if let Some(last) = last_change {
            if last.elapsed() >= debounce
                && (!pending_changed.is_empty() || !pending_removed.is_empty())
            {
                let mut changed: Vec<NormalizedPath> = pending_changed.drain().collect();
                let mut removed: Vec<NormalizedPath> = pending_removed.drain().collect();
                changed.sort();
                removed.sort();
                if batch_tx
                    .send(PollWatchBatch {
                        changed,
                        removed,
                        overflow: false,
                    })
                    .is_err()
                {
                    break;
                }
                last_change = None;
            }
        }

        snapshot = current;

        if shutdown_rx.recv_timeout(poll_interval).is_ok() {
            break;
        }
    }
}

fn build_config(
    root: &Path,
    include_folders: &[NormalizedPath],
    include_globs: &[String],
    excluded_patterns: &[String],
) -> std::io::Result<ScanConfig> {
    let root = NormalizedPath::new(root.canonicalize()?);

    let include_folders = if include_folders.is_empty() {
        vec![root.clone()]
    } else {
        include_folders
            .iter()
            .map(|folder| {
                let absolute = if folder.is_absolute() {
                    folder.clone().into_path_buf()
                } else {
                    root.join(folder).into_path_buf()
                };
                Ok(NormalizedPath::new(
                    absolute.canonicalize().unwrap_or(absolute),
                ))
            })
            .collect::<std::io::Result<Vec<_>>>()?
    };

    let include_patterns = if include_globs.is_empty() {
        vec!["**".to_string()]
    } else {
        include_globs.to_vec()
    };
    let include_globs = build_globset(&expand_patterns(&include_patterns))?;

    let excluded_names = excluded_patterns
        .iter()
        .filter(|pattern| !has_glob_meta(pattern) && !pattern.contains('/'))
        .cloned()
        .collect::<HashSet<_>>();
    let exclude_globs = build_globset(&expand_patterns(excluded_patterns))?;

    Ok(ScanConfig {
        root,
        include_folders,
        include_globs,
        exclude_globs,
        excluded_names,
    })
}

fn build_globset(patterns: &[String]) -> std::io::Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(
            Glob::new(pattern).map_err(|e| std::io::Error::other(format!("invalid glob: {e}")))?,
        );
    }
    builder
        .build()
        .map_err(|e| std::io::Error::other(format!("failed to compile glob set: {e}")))
}

fn expand_patterns(patterns: &[String]) -> Vec<String> {
    let mut expanded = Vec::new();
    for pattern in patterns {
        let mut seen = HashSet::new();
        let mut pending = vec![pattern.replace('\\', "/")];
        while let Some(current) = pending.pop() {
            if !seen.insert(current.clone()) {
                continue;
            }
            if current.contains("**/") {
                pending.push(current.replace("**/", ""));
            }
            if current.contains("/**") {
                pending.push(current.replace("/**", ""));
            }
            expanded.push(current);
        }
    }
    expanded
}

fn has_glob_meta(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?') || pattern.contains('[')
}

fn scan_snapshot(config: &ScanConfig) -> HashMap<NormalizedPath, FileState> {
    use rayon::prelude::*;

    let mut result = HashMap::new();

    for base in &config.include_folders {
        if !base.exists() {
            continue;
        }

        let root = config.root.clone();
        let exclude_names = config.excluded_names.clone();
        let exclude_globs = config.exclude_globs.clone();

        let walker = jwalk::WalkDir::new(base)
            .follow_links(false)
            .skip_hidden(false)
            .process_read_dir(move |_depth, _path, _state, children| {
                children.retain(|entry| {
                    let Ok(entry) = entry else {
                        return true;
                    };
                    if !entry.file_type.is_dir() {
                        return true;
                    }
                    let path = entry.path();
                    if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                        if exclude_names.contains(name) {
                            return false;
                        }
                    }
                    let rel = rel_string(&root, &path);
                    !exclude_globs.is_match(&rel)
                });
            });

        // Step 1: collect the candidate file paths from the (already-parallel)
        // jwalk traversal. Applying the include/exclude globs here is cheap
        // (string match) and avoids an extra `metadata()` syscall for files
        // we'd just drop. Normalize at collection time so step 2's parallel
        // metadata fetch already operates on the watcher's canonical key
        // type.
        let candidates: Vec<NormalizedPath> = walker
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                if !entry.file_type.is_file() {
                    return None;
                }
                let path = entry.path();
                let rel = rel_string(&config.root, &path);
                if config.exclude_globs.is_match(&rel) || !config.include_globs.is_match(&rel) {
                    return None;
                }
                Some(NormalizedPath::new(&path))
            })
            .collect();

        // Step 2: fetch metadata in parallel. Each `metadata()` is an
        // independent syscall — on Windows this is the dominant cost
        // (Defender intercepts every stat). Skip files whose metadata
        // can't be read (deleted between walk and stat).
        let pairs: Vec<(NormalizedPath, FileState)> = candidates
            .par_iter()
            .filter_map(|path| {
                let metadata = path.metadata().ok()?;
                Some((
                    path.clone(),
                    FileState {
                        mtime_ns: metadata
                            .modified()
                            .ok()
                            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                            .map_or(0, |duration| duration.as_nanos()),
                        size: metadata.len(),
                    },
                ))
            })
            .collect();

        result.extend(pairs);
    }

    result
}

fn diff_snapshots(
    previous: &HashMap<NormalizedPath, FileState>,
    current: &HashMap<NormalizedPath, FileState>,
) -> (HashSet<NormalizedPath>, HashSet<NormalizedPath>) {
    let mut changed = HashSet::new();
    let mut removed = HashSet::new();

    for (path, state) in current {
        if previous.get(path) != Some(state) {
            changed.insert(path.clone());
        }
    }

    for path in previous.keys() {
        if !current.contains_key(path) {
            removed.insert(path.clone());
        }
    }

    (changed, removed)
}

fn rel_string(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

    fn wait_for_batch(watcher: &PollingWatcher) -> PollWatchBatch {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(batch) = watcher
                .poll_timeout(Duration::from_millis(100))
                .expect("poll should succeed")
            {
                return batch;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for watcher batch"
            );
        }
    }

    #[test]
    fn polling_watcher_reports_filtered_changes() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("build")).unwrap();
        fs::write(root.join("src/watch.cpp"), "a\n").unwrap();
        fs::write(root.join("build/ignore.cpp"), "a\n").unwrap();

        let mut config = PollingWatcherConfig::new(root);
        config.include_folders = vec![NormalizedPath::from("src"), NormalizedPath::from("build")];
        config.include_globs = vec!["**/*.cpp".to_string()];
        config.excluded_patterns = vec!["build".to_string()];
        config.poll_interval = Duration::from_millis(20);
        config.debounce = Duration::from_millis(20);

        let watcher = PollingWatcher::new(config).unwrap();
        watcher.start().unwrap();
        fs::write(root.join("src/watch.cpp"), "b\n").unwrap();
        fs::write(root.join("build/ignore.cpp"), "b\n").unwrap();

        let batch = wait_for_batch(&watcher);
        watcher.stop().unwrap();

        assert_eq!(
            batch.changed,
            vec![NormalizedPath::new(
                root.join("src/watch.cpp").canonicalize().unwrap(),
            )]
        );
        assert!(batch.removed.is_empty());
    }

    #[test]
    fn polling_watcher_resume_resets_baseline() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("watch.cpp"), "a\n").unwrap();

        let mut config = PollingWatcherConfig::new(root);
        config.include_globs = vec!["**/*.cpp".to_string()];
        config.poll_interval = Duration::from_millis(20);
        config.debounce = Duration::from_millis(20);

        let watcher = PollingWatcher::new(config).unwrap();
        watcher.start().unwrap();
        watcher.stop().unwrap();
        fs::write(root.join("watch.cpp"), "b\n").unwrap();
        watcher.resume().unwrap();
        assert!(watcher
            .poll_timeout(Duration::from_millis(200))
            .unwrap()
            .is_none());
        fs::write(root.join("watch.cpp"), "c\n").unwrap();
        let batch = wait_for_batch(&watcher);
        watcher.stop().unwrap();

        assert_eq!(
            batch.changed,
            vec![NormalizedPath::new(
                root.join("watch.cpp").canonicalize().unwrap()
            )]
        );
    }

    #[test]
    fn polling_watcher_callbacks_and_polling_share_events() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("watch.cpp"), "a\n").unwrap();

        let mut config = PollingWatcherConfig::new(root);
        config.include_globs = vec!["**/*.cpp".to_string()];
        config.poll_interval = Duration::from_millis(20);
        config.debounce = Duration::from_millis(20);

        let watcher = PollingWatcher::new(config).unwrap();
        let callback_count = Arc::new(AtomicUsize::new(0));
        let callback_count_clone = Arc::clone(&callback_count);
        watcher
            .add_callback(move |_batch| {
                callback_count_clone.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
        watcher.start().unwrap();

        fs::write(root.join("watch.cpp"), "b\n").unwrap();
        let batch = wait_for_batch(&watcher);
        watcher.stop().unwrap();

        assert_eq!(callback_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            batch.changed,
            vec![NormalizedPath::new(
                root.join("watch.cpp").canonicalize().unwrap()
            )]
        );
    }
}

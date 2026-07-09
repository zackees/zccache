//! PyO3 bindings for the polling watcher.

use std::path::PathBuf;
use std::time::Duration;

use pyo3::exceptions::{PyOSError, PyRuntimeError};
use pyo3::prelude::*;

use crate::{PollWatchBatch, PollingWatcher, PollingWatcherConfig};

fn io_to_py_err(e: std::io::Error) -> PyErr {
    PyErr::new::<PyOSError, _>(e.to_string())
}

fn runtime_to_py_err(message: impl Into<String>) -> PyErr {
    PyErr::new::<PyRuntimeError, _>(message.into())
}

#[pyclass(module = "zccache.watcher._native")]
#[derive(Clone, Debug)]
pub struct WatchBatch {
    #[pyo3(get)]
    changed: Vec<String>,
    #[pyo3(get)]
    removed: Vec<String>,
    #[pyo3(get)]
    overflow: bool,
}

impl From<PollWatchBatch> for WatchBatch {
    fn from(value: PollWatchBatch) -> Self {
        Self {
            changed: value
                .changed
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            removed: value
                .removed
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            overflow: value.overflow,
        }
    }
}

#[pyclass(module = "zccache.watcher._native")]
pub struct NativeWatcher {
    watcher: PollingWatcher,
}

unsafe impl Send for NativeWatcher {}

#[pymethods]
impl NativeWatcher {
    #[new]
    #[pyo3(signature = (
        root,
        include_folders=vec![],
        include_globs=vec![],
        excluded_patterns=vec![],
        poll_interval_ms=100,
        debounce_ms=200
    ))]
    fn new(
        root: String,
        include_folders: Vec<String>,
        include_globs: Vec<String>,
        excluded_patterns: Vec<String>,
        poll_interval_ms: u64,
        debounce_ms: u64,
    ) -> PyResult<Self> {
        let mut config = PollingWatcherConfig::new(PathBuf::from(root));
        config.include_folders = include_folders.into_iter().map(Into::into).collect();
        config.include_globs = include_globs;
        config.excluded_patterns = excluded_patterns;
        config.poll_interval = Duration::from_millis(poll_interval_ms.max(1));
        config.debounce = Duration::from_millis(debounce_ms);

        let watcher = PollingWatcher::new(config).map_err(io_to_py_err)?;
        Ok(Self { watcher })
    }

    fn start(&self) -> PyResult<()> {
        self.watcher.start().map_err(io_to_py_err)
    }

    fn stop(&self) -> PyResult<()> {
        self.watcher.stop().map_err(io_to_py_err)
    }

    fn resume(&self) -> PyResult<()> {
        self.watcher.resume().map_err(io_to_py_err)
    }

    fn is_running(&self) -> bool {
        self.watcher.is_running()
    }

    #[pyo3(signature = (timeout_ms=0))]
    fn poll_batch(&self, timeout_ms: u64) -> PyResult<Option<WatchBatch>> {
        let batch = self
            .watcher
            .poll_timeout(Duration::from_millis(timeout_ms))
            .map_err(|_| runtime_to_py_err("watcher polling failed"))?;
        Ok(batch.map(WatchBatch::from))
    }
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<WatchBatch>()?;
    m.add_class::<NativeWatcher>()?;
    Ok(())
}

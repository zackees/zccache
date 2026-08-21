//! Python `zccache.exec_cached` bridge (#1433).

use std::path::PathBuf;

use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use zccache::cli::{client_exec_probe, client_exec_store};

use super::runtime_to_py_err;

/// Cache one caller-owned Python computation through the daemon.
///
/// IPC and persistent-store waits run without the GIL. The GIL is held only
/// while invoking the supplied Python callback and copying its returned bytes.
#[pyfunction]
#[pyo3(signature = (name, input_files, input_env, extra_key, runner))]
fn exec_cached(
    py: Python<'_>,
    name: String,
    input_files: Vec<String>,
    input_env: Vec<(String, String)>,
    extra_key: Vec<u8>,
    runner: Py<PyAny>,
) -> PyResult<Py<PyBytes>> {
    let input_files: Vec<PathBuf> = input_files.into_iter().map(PathBuf::from).collect();
    let probe = py
        .allow_threads(|| {
            client_exec_probe(None, &name, &input_files, &input_env, extra_key.as_slice())
        })
        .map_err(runtime_to_py_err)?;

    if let Some(bytes) = probe.cached_bytes {
        return Ok(PyBytes::new(py, &bytes).unbind());
    }

    let result = runner.call0(py)?;
    let result = result
        .bind(py)
        .downcast::<PyBytes>()
        .map_err(|_| PyTypeError::new_err("exec_cached runner must return bytes"))?;
    let result_bytes = result.as_bytes().to_vec();

    py.allow_threads(|| client_exec_store(None, &probe.cache_key_hex, result_bytes.as_slice()))
        .map_err(runtime_to_py_err)?;

    Ok(PyBytes::new(py, &result_bytes).unbind())
}

pub(super) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(exec_cached, m)?)
}

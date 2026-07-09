//! PyO3 cdylib for the fingerprint engine.
//!
//! The Rust fingerprint engine lives in this crate; this module hosts the
//! `_native` Python extension module so the polished `zccache.fingerprint`
//! Python package can `import zccache.fingerprint._native`.

use pyo3::prelude::*;

use crate::error::FingerprintError;
use crate::hash_cache::compute_aggregate_hash;
use crate::scan::{self, ScannedFile};

fn to_py_err(e: FingerprintError) -> PyErr {
    PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string())
}

fn io_to_py_err(e: std::io::Error) -> PyErr {
    PyErr::new::<pyo3::exceptions::PyOSError, _>(e.to_string())
}

/// Walk files by extension filter and compute an aggregate blake3 hash.
#[pyfunction]
#[pyo3(signature = (root, extensions=vec![], exclude_dirs=vec![]))]
fn hash_files(root: &str, extensions: Vec<String>, exclude_dirs: Vec<String>) -> PyResult<String> {
    let ext_refs: Vec<&str> = extensions.iter().map(String::as_str).collect();
    let dir_refs: Vec<&str> = exclude_dirs.iter().map(String::as_str).collect();
    let files = scan::walk_files(root.as_ref(), &ext_refs, &dir_refs).map_err(to_py_err)?;
    compute_aggregate_hash(&files).map_err(to_py_err)
}

/// Walk files by glob patterns and compute an aggregate blake3 hash.
#[pyfunction]
#[pyo3(signature = (root, include=vec![], exclude=vec![]))]
fn hash_files_glob(root: &str, include: Vec<String>, exclude: Vec<String>) -> PyResult<String> {
    let inc_refs: Vec<&str> = include.iter().map(String::as_str).collect();
    let exc_refs: Vec<&str> = exclude.iter().map(String::as_str).collect();
    let files = scan::walk_files_glob(root.as_ref(), &inc_refs, &exc_refs).map_err(to_py_err)?;
    compute_aggregate_hash(&files).map_err(to_py_err)
}

/// Walk files by extension filter and return per-file blake3 hashes.
///
/// Returns a list of `(relative_path, blake3_hex)` tuples.
#[pyfunction]
#[pyo3(signature = (root, extensions=vec![], exclude_dirs=vec![]))]
fn walk_and_hash(
    root: &str,
    extensions: Vec<String>,
    exclude_dirs: Vec<String>,
) -> PyResult<Vec<(String, String)>> {
    let ext_refs: Vec<&str> = extensions.iter().map(String::as_str).collect();
    let dir_refs: Vec<&str> = exclude_dirs.iter().map(String::as_str).collect();
    let files = scan::walk_files(root.as_ref(), &ext_refs, &dir_refs).map_err(to_py_err)?;
    hash_each_file(&files)
}

fn hash_each_file(files: &[ScannedFile]) -> PyResult<Vec<(String, String)>> {
    let mut result = Vec::with_capacity(files.len());
    for file in files {
        let hash = zccache_hash::hash_file(&file.absolute).map_err(io_to_py_err)?;
        result.push((file.relative.clone(), hash.to_hex()));
    }
    Ok(result)
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(hash_files, m)?)?;
    m.add_function(wrap_pyfunction!(hash_files_glob, m)?)?;
    m.add_function(wrap_pyfunction!(walk_and_hash, m)?)?;
    Ok(())
}

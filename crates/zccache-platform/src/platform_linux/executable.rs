//! Linux executable naming and `PATH` lookup.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

pub fn native_name(stem: &OsStr) -> OsString {
    stem.to_os_string()
}

pub fn native_library_name(stem: &OsStr) -> OsString {
    let mut name = stem.to_os_string();
    if std::path::Path::new(stem).extension().is_none() {
        name.push(".so");
    }
    name
}

pub fn clang_library_candidates() -> Vec<PathBuf> {
    [
        "/usr/lib/llvm-18/lib/libclang.so",
        "/usr/lib/llvm-17/lib/libclang.so",
        "/usr/lib/llvm-16/lib/libclang.so",
        "/usr/lib/libclang.so",
        "/usr/local/lib/libclang.so",
    ]
    .map(PathBuf::from)
    .to_vec()
}

pub fn find_in_paths(name: &OsStr, directories: &[PathBuf]) -> Option<PathBuf> {
    directories
        .iter()
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

pub fn stem_matches(path: &OsStr, expected: &str) -> bool {
    std::path::Path::new(path).file_stem() == Some(OsStr::new(expected))
}

pub fn unlock_for_replacement(_: &std::path::Path) -> std::io::Result<bool> {
    Ok(false)
}

//! Linux executable naming and `PATH` lookup.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

pub fn native_name(stem: &OsStr) -> OsString {
    stem.to_os_string()
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

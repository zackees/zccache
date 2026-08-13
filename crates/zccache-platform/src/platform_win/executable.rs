//! Windows executable naming and `PATH`/`PATHEXT` lookup.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

pub fn native_name(stem: &OsStr) -> OsString {
    if std::path::Path::new(stem).extension().is_some() {
        stem.to_os_string()
    } else {
        let mut name = stem.to_os_string();
        name.push(".exe");
        name
    }
}

pub fn native_library_name(stem: &OsStr) -> OsString {
    let mut name = stem.to_os_string();
    if std::path::Path::new(stem).extension().is_none() {
        name.push(".dll");
    }
    name
}

pub fn clang_library_candidates() -> Vec<PathBuf> {
    [
        r"C:\Program Files\LLVM\bin\libclang.dll",
        r"C:\Program Files\LLVM\lib\libclang.dll",
        r"C:\Program Files\doxygen\bin\libclang.dll",
    ]
    .map(PathBuf::from)
    .to_vec()
}

pub fn find_in_paths(name: &OsStr, directories: &[PathBuf]) -> Option<PathBuf> {
    let path = std::path::Path::new(name);
    let suffixes: Vec<OsString> = if path.extension().is_some() {
        vec![OsString::new()]
    } else {
        std::env::var_os("PATHEXT")
            .map(|value| {
                value
                    .to_string_lossy()
                    .split(';')
                    .filter(|suffix| !suffix.is_empty())
                    .map(OsString::from)
                    .collect()
            })
            .filter(|suffixes: &Vec<OsString>| !suffixes.is_empty())
            .unwrap_or_else(|| [".COM", ".EXE", ".BAT", ".CMD"].map(OsString::from).to_vec())
    };

    directories.iter().find_map(|directory| {
        suffixes.iter().find_map(|suffix| {
            let mut candidate_name = name.to_os_string();
            candidate_name.push(suffix);
            let candidate = directory.join(candidate_name);
            candidate.is_file().then_some(candidate)
        })
    })
}

pub fn stem_matches(path: &OsStr, expected: &str) -> bool {
    std::path::Path::new(path)
        .file_stem()
        .and_then(OsStr::to_str)
        .is_some_and(|stem| stem.eq_ignore_ascii_case(expected))
}

pub fn unlock_for_replacement(image: &std::path::Path) -> std::io::Result<bool> {
    let nonce = std::process::id()
        ^ std::time::UNIX_EPOCH
            .elapsed()
            .unwrap_or_default()
            .subsec_nanos();
    let retired = image.with_extension(format!("exe.old.{nonce}"));
    std::fs::rename(image, &retired)?;
    let _ = std::fs::copy(&retired, image);
    Ok(true)
}

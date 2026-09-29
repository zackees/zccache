//! Guard for #1771: `persist/mtime.rs` is the only daemon-core production
//! file that sets a file time. A materialized output's mtime is one contract
//! across LINK, REFLINK and COPY; a stray `set_file_mtime` elsewhere is how a
//! second, divergent policy gets in.

use std::path::{Path, PathBuf};

const OWNER: &str = "daemon/server/persist/mtime.rs";

/// Spelled with `concat!` so this file never contains a needle itself.
const NEEDLES: &[&str] = &[
    concat!("set_file_", "mtime"),
    concat!("set_", "modified"),
    concat!("File", "Times"),
    concat!("file", "time::"),
    concat!("set_", "times("),
    concat!("futim", "ens"),
    concat!("utimens", "at"),
    concat!("SetFile", "Time"),
];

/// `(path relative to src/, production source)` of every daemon-core `.rs`
/// file: test files and directories are skipped, and an inline
/// `#[cfg(test)] mod` (by convention the tail of its file) is cut off.
fn production_sources() -> Vec<(String, String)> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut sources = Vec::new();
    let mut stack: Vec<PathBuf> = vec![src.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                if name != "tests" && !name.contains("test") {
                    stack.push(path);
                }
                continue;
            }
            if !name.ends_with(".rs") || name.contains("test") {
                continue;
            }
            let relative = path
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&path).unwrap();
            sources.push((relative, strip_inline_test_module(&text)));
        }
    }
    sources
}

fn strip_inline_test_module(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    for (index, line) in lines.iter().enumerate() {
        if line.trim() != "#[cfg(test)]" {
            continue;
        }
        let next = lines.get(index + 1).map_or("", |next| next.trim_start());
        if next.starts_with("mod ") || next.starts_with("pub(crate) mod ") {
            return lines[..index].join("\n");
        }
    }
    text.to_owned()
}

#[test]
fn only_persist_mtime_sets_file_times() {
    let offenders: Vec<String> = production_sources()
        .into_iter()
        .filter(|(relative, source)| {
            relative != OWNER && NEEDLES.iter().any(|needle| source.contains(needle))
        })
        .map(|(relative, _)| relative)
        .collect();
    assert!(
        offenders.is_empty(),
        "set file times through persist/mtime.rs, the single owner of materialized-output \
         mtime decisions (#1771): {offenders:?}"
    );
}

#[test]
fn the_owner_is_scanned_and_really_sets_times() {
    // Keeps the guard honest: a moved or renamed owner would otherwise turn
    // the scan above into a vacuous pass.
    let sources = production_sources();
    let owner = sources
        .iter()
        .find(|(relative, _)| relative == OWNER)
        .expect("persist/mtime.rs must exist where the guard expects it");
    assert!(NEEDLES.iter().any(|needle| owner.1.contains(needle)));
}

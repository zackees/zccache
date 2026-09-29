//! Guard for #1771, workspace-wide: `zccache-core/src/mtime.rs` is the only
//! production file that sets the mtime of a materialized output. A second
//! writer is how a divergent policy (one mode stamping `now()`, another
//! preserving the object) gets in, so any other file that sets a file time
//! must be on [`ALLOWLIST`] with the reason it is not a materialized output.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

const OWNER: &str = "zccache-core/src/mtime.rs";

/// `(file relative to crates/, why it may set a file time)`. Every entry is a
/// file that is NOT delivering a cached output.
const ALLOWLIST: &[(&str, &str)] = &[
    (
        "zccache-fingerprint/src/mtime_replay/mod.rs",
        "#1595 source-mtime replay restores workspace SOURCE files' recorded mtimes, not outputs",
    ),
    (
        "zccache-cli-core/src/cli/snapshot_fp.rs",
        "bumps existing cargo fingerprint stamp files in place after validating sources; nothing is materialized",
    ),
    (
        "zccache-core/src/config/retired_store.rs",
        "`.last-active` liveness marker for the retired-store sweeper, not a cached output",
    ),
];

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
    concat!("set_symlink_file_", "times"),
];

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// `(path relative to crates/, production source)` of every workspace `.rs`
/// file under a crate's `src/` (or `build.rs`). Test, bench and example files
/// and directories are skipped, an inline `#[cfg(test)] mod` (by convention
/// the tail of its file) is cut off, and comment lines are ignored.
fn production_sources() -> Vec<(String, String)> {
    let root = crates_dir();
    let mut sources = Vec::new();
    let mut stack: Vec<PathBuf> = Vec::new();
    for krate in std::fs::read_dir(&root).unwrap().flatten() {
        // The test-support crate is test infrastructure by definition.
        if krate.file_name() == "zccache-test-support" {
            continue;
        }
        stack.push(krate.path().join("src"));
        let build = krate.path().join("build.rs");
        if build.is_file() {
            stack.push(build);
        }
    }
    while let Some(path) = stack.pop() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if path.is_dir() {
            if name.contains("test") {
                continue;
            }
            for entry in std::fs::read_dir(&path).unwrap().flatten() {
                stack.push(entry.path());
            }
            continue;
        }
        if !name.ends_with(".rs") || name.contains("test") {
            continue;
        }
        let relative = path
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let text = std::fs::read_to_string(&path).unwrap();
        sources.push((relative, code_only(&strip_inline_test_module(&text))));
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

fn code_only(text: &str) -> String {
    text.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn sets_file_time(source: &str) -> bool {
    NEEDLES.iter().any(|needle| source.contains(needle))
}

#[test]
fn only_the_owner_and_the_allowlist_set_file_times() {
    let offenders: Vec<String> = production_sources()
        .into_iter()
        .filter(|(relative, source)| {
            relative != OWNER
                && sets_file_time(source)
                && !ALLOWLIST.iter().any(|(allowed, _)| allowed == relative)
        })
        .map(|(relative, _)| relative)
        .collect();
    assert!(
        offenders.is_empty(),
        "route file-time writes through zccache_core::mtime, the single owner of \
         materialized-output mtime decisions (#1771), or allowlist the file with a \
         reason it is not a materialized output: {offenders:?}"
    );
}

#[test]
fn the_owner_and_every_allowlist_entry_still_set_times() {
    // Keeps the guard honest: a moved file or an entry that no longer sets a
    // time would otherwise turn the scan above into a vacuous pass.
    let sources = production_sources();
    for path in std::iter::once(OWNER).chain(ALLOWLIST.iter().map(|(path, _)| *path)) {
        let (_, source) = sources
            .iter()
            .find(|(relative, _)| relative == path)
            .unwrap_or_else(|| panic!("{path} must exist where the guard expects it"));
        assert!(
            sets_file_time(source),
            "{path} no longer sets a file time; drop it from the guard"
        );
    }
    assert!(ALLOWLIST.iter().all(|(_, reason)| !reason.is_empty()));
}

//! System include path discovery from compiler output.
//!
//! Parses the output of `<compiler> -v -E -x c++ /dev/null 2>&1` to extract
//! the compiler's default system include search paths. These paths are used
//! to resolve `#include <...>` directives that don't match any explicit
//! `-I`/`-isystem` paths.
//!
//! The discovery command differs by platform:
//! - Linux/macOS: `<compiler> -v -E -x c++ /dev/null 2>&1`
//! - Windows: `<compiler> -v -E -x c++ NUL 2>&1`
//!
//! The actual command execution is left to the caller (daemon). This module
//! only handles parsing the output and caching results.
//!
//! ## On-disk persistence (issue #541)
//!
//! Spawning the compiler with `-v -E` to scrape `#include <...>` lines costs
//! ~30-50 ms (Linux, more on Windows) — paid on every first-after-restart
//! C/C++ compile of a daemon. The cache persists `(compiler_path, mtime,
//! size) -> include_paths` to disk alongside `metadata.bin` so subsequent
//! daemon runs against the same toolchain skip the spawn entirely. Stat
//! verification on load catches in-place compiler upgrades (apt upgrade,
//! Homebrew `brew upgrade clang`, etc.) — a mismatch silently rediscovers,
//! so a stale snapshot cannot poison the cache.

use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::core::NormalizedPath;

/// Parse compiler `-v -E` output to extract system include paths.
///
/// Looks for the section between `#include <...> search starts here:`
/// and `End of search list.` in the compiler's stderr output.
///
/// Each line in that section is trimmed and treated as a directory path.
/// Lines starting with ` (framework directory)` are included but the
/// suffix is stripped.
#[must_use]
pub fn parse_system_include_output(output: &str) -> Vec<NormalizedPath> {
    let mut in_section = false;
    let mut paths = Vec::new();

    for line in output.lines() {
        let trimmed = line.trim();

        if trimmed == "#include <...> search starts here:" {
            in_section = true;
            continue;
        }

        if trimmed == "End of search list." {
            break;
        }

        if in_section && !trimmed.is_empty() {
            // Some compilers annotate framework dirs: "/path (framework directory)"
            let path_str = if let Some(stripped) = trimmed.strip_suffix(" (framework directory)") {
                stripped
            } else {
                trimmed
            };

            if !path_str.is_empty() {
                paths.push(path_str.into());
            }
        }
    }

    paths
}

/// Build the compiler discovery command arguments.
///
/// Returns the arguments to pass to the compiler to discover system include
/// paths. The caller should execute the compiler with these args and capture
/// stderr.
#[must_use]
pub fn discovery_args() -> Vec<&'static str> {
    if cfg!(windows) {
        vec!["-v", "-E", "-x", "c++", "NUL"]
    } else {
        vec!["-v", "-E", "-x", "c++", "/dev/null"]
    }
}

/// Build the **fast** clang-family discovery command arguments.
///
/// Issue #541 option (B): `clang -###` makes clang print the `-cc1`
/// command-line it WOULD execute, then exit without spawning cc1. The
/// printed line includes `-internal-isystem` / `-internal-externc-isystem`
/// flags pointing at every system include path the driver would have
/// passed to the real preprocessor. Wall-clock: ~3-5 ms vs the ~30-50 ms
/// of the full `-v -E` discovery.
///
/// Only safe for clang-family compilers (clang, clang++, em++,
/// clang-cl). Gcc accepts `-###` but emits a different format that
/// doesn't include the per-path `-internal-isystem` flags this parser
/// expects. Use [`discovery_args`] (the slow path) for gcc/MSVC.
#[must_use]
pub fn discovery_args_fast() -> Vec<&'static str> {
    if cfg!(windows) {
        vec!["-###", "-E", "-x", "c++", "NUL"]
    } else {
        vec!["-###", "-E", "-x", "c++", "/dev/null"]
    }
}

/// Parse the `-cc1` line from clang's `-###` output and extract every
/// `-internal-isystem` / `-internal-externc-isystem` path.
///
/// The driver prints exactly one line per invocation that looks like:
///
/// ```text
///  "/usr/lib/llvm-18/bin/clang-18" "-cc1" "-triple" "x86_64-pc-linux-gnu" "-E" \
///    "-internal-isystem" "/usr/lib/llvm-18/lib/clang/18/include" \
///    "-internal-isystem" "/usr/local/include" \
///    "-internal-externc-isystem" "/usr/include/x86_64-linux-gnu" \
///    "-internal-externc-isystem" "/usr/include" ...
/// ```
///
/// Tokens are double-quoted; spaces inside paths are preserved by the
/// quoting. The tokenizer splits on `"` and reads odd-indexed segments
/// (the actual argument values, alternating with single-space gaps).
///
/// Returns the include paths in driver-emitted order (which matches
/// clang's actual `#include <...>` search order — the first `-cc1`
/// invocation's order is canonical).
#[must_use]
pub fn parse_cc1_system_include_output(output: &str) -> Vec<NormalizedPath> {
    let mut paths = Vec::new();
    for line in output.lines() {
        // Cheap shape probe — the cc1 line always contains "-cc1" as a
        // quoted token. Skip everything else (version banner, "Found
        // candidate GCC" notes, etc.).
        if !line.contains("\"-cc1\"") {
            continue;
        }
        let tokens = tokenize_quoted(line);
        let mut iter = tokens.iter();
        while let Some(token) = iter.next() {
            if *token == "-internal-isystem" || *token == "-internal-externc-isystem" {
                if let Some(path) = iter.next() {
                    if !path.is_empty() {
                        paths.push(NormalizedPath::new(*path));
                    }
                }
            }
        }
        // Only the first cc1 line — multi-arch / multi-tu drivers may
        // emit several but the include search order is identical.
        if !paths.is_empty() {
            break;
        }
    }
    paths
}

/// Split a `-###`-style quoted command line on `"` and return the
/// odd-indexed segments (the actual argument values). Empty even-index
/// segments are the inter-quote whitespace; even-index non-empty
/// segments would indicate malformed quoting and are dropped.
fn tokenize_quoted(line: &str) -> Vec<&str> {
    line.split('"')
        .enumerate()
        .filter_map(|(i, s)| if i % 2 == 1 { Some(s) } else { None })
        .collect()
}

/// On-disk format version for the persisted `SystemIncludeCache` snapshot.
///
/// Bump on any layout change so the loader rejects older / newer snapshots
/// instead of mis-decoding them.
pub const FORMAT_VERSION: u32 = 1;

/// One cache entry — the discovered include paths plus the (mtime, size)
/// fingerprint of the compiler binary that produced them. Verifying stat
/// on lookup catches in-place compiler upgrades that would otherwise
/// serve stale include paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemIncludeEntry {
    pub mtime: SystemTime,
    pub size: u64,
    pub paths: Vec<NormalizedPath>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedSystemIncludes {
    version: u32,
    entries: Vec<(NormalizedPath, SystemIncludeEntry)>,
}

/// Cache of discovered system include paths, keyed by compiler path.
///
/// Avoids re-running the compiler discovery command for the same compiler
/// across sessions. Entries store a (mtime, size) fingerprint of the
/// compiler binary; stat-verify on lookup rediscovers if the binary changed.
#[derive(Debug, Default)]
pub struct SystemIncludeCache {
    cache: HashMap<NormalizedPath, SystemIncludeEntry>,
}

impl SystemIncludeCache {
    /// Create an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up cached system include paths for a compiler, verifying stat.
    ///
    /// Returns `None` when either the cache has no entry OR the compiler
    /// binary's (mtime, size) no longer matches the entry's fingerprint.
    /// Stat errors (e.g., compiler removed) also return `None` so the
    /// caller falls through to rediscovery.
    #[must_use]
    pub fn get(&self, compiler: &Path) -> Option<&[NormalizedPath]> {
        let key = NormalizedPath::new(compiler);
        let entry = self.cache.get(&key)?;
        let metadata = std::fs::metadata(compiler).ok()?;
        let mtime = metadata.modified().ok()?;
        let size = metadata.len();
        if entry.mtime == mtime && entry.size == size {
            Some(entry.paths.as_slice())
        } else {
            None
        }
    }

    /// Store discovered system include paths for a compiler.
    ///
    /// Captures the compiler binary's (mtime, size) at insert time so a
    /// subsequent `get` can stat-verify. If the stat fails (compiler
    /// removed mid-insert), the entry is silently dropped — better to
    /// re-discover than to cache without a valid fingerprint.
    pub fn insert(&mut self, compiler: NormalizedPath, paths: Vec<NormalizedPath>) {
        let Ok(metadata) = std::fs::metadata(compiler.as_path()) else {
            return;
        };
        let Ok(mtime) = metadata.modified() else {
            return;
        };
        let size = metadata.len();
        self.cache
            .insert(compiler, SystemIncludeEntry { mtime, size, paths });
    }

    /// Get cached paths or discover them using the provided closure.
    ///
    /// Performs the same stat-verify as `get`. On a verified hit, the
    /// discovery closure is not invoked. On a miss (no entry OR stat
    /// mismatch), the closure runs and its result is cached with a
    /// fresh fingerprint.
    pub fn get_or_discover<F>(&mut self, compiler: &Path, discover: F) -> &[NormalizedPath]
    where
        F: FnOnce(&Path) -> Vec<NormalizedPath>,
    {
        let compiler_key = NormalizedPath::new(compiler);
        let stat_match = self.cache.get(&compiler_key).is_some_and(|entry| {
            std::fs::metadata(compiler)
                .ok()
                .and_then(|m| m.modified().ok().map(|mt| (mt, m.len())))
                .is_some_and(|(mt, size)| mt == entry.mtime && size == entry.size)
        });
        if !stat_match {
            let paths = discover(compiler);
            self.insert(compiler_key.clone(), paths);
        }
        self.cache
            .get(&compiler_key)
            .map(|e| e.paths.as_slice())
            .unwrap_or(&[])
    }

    /// Remove all cached entries.
    pub fn clear(&mut self) {
        self.cache.clear();
    }

    /// Number of cached entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Check if the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// Persist the cache to `path` as a versioned bincode snapshot.
    ///
    /// Atomic on success: writes to `<path>.tmp-<pid>`, then renames over
    /// `path`. Empty snapshots short-circuit without touching disk. Stale
    /// entries on disk are harmless: `get` re-stats every key before
    /// trusting the cached entry, so a mismatch silently downgrades to a
    /// re-discovery on the daemon side.
    ///
    /// # Errors
    ///
    /// Returns I/O errors from `create_dir_all`, `write`, `rename`, or
    /// bincode serialization.
    pub fn save_to_disk(&self, path: &Path) -> std::io::Result<()> {
        let entries: Vec<(NormalizedPath, SystemIncludeEntry)> = self
            .cache
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        if entries.is_empty() {
            tracing::debug!(
                path = %path.display(),
                "system include cache flush: 0 entries, skipping write"
            );
            return Ok(());
        }

        let entry_count = entries.len();
        let snapshot = PersistedSystemIncludes {
            version: FORMAT_VERSION,
            entries,
        };
        let bytes = bincode::serialize(&snapshot)
            .map_err(|e| std::io::Error::other(format!("bincode serialize: {e}")))?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "system_includes.bin".into());
        let tmp = path.with_file_name(format!(".{name}.tmp-{}", std::process::id()));

        let result = write_atomic_durable(&tmp, path, &bytes);
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        if result.is_ok() {
            tracing::info!(
                path = %path.display(),
                entries = entry_count,
                bytes = bytes.len(),
                "system include cache flushed to disk"
            );
        }
        result
    }

    /// Load a previously persisted snapshot from `path`.
    ///
    /// Returns an empty cache when the file is absent (first run). Any
    /// other I/O error, bincode decode failure, or version mismatch is
    /// surfaced as `Err`; the daemon caller is expected to log and start
    /// empty. Stat-verification at `get` re-checks every loaded entry,
    /// so a stale on-disk snapshot cannot produce incorrect includes.
    ///
    /// # Errors
    ///
    /// Any I/O error other than `NotFound`, any bincode decode failure,
    /// or any version mismatch.
    pub fn load_from_disk(path: &Path) -> std::io::Result<Self> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(
                    path = %path.display(),
                    "system include cache file not found, starting empty"
                );
                return Ok(Self::new());
            }
            Err(e) => return Err(e),
        };

        let snapshot: PersistedSystemIncludes = bincode::deserialize(&bytes)
            .map_err(|e| std::io::Error::other(format!("bincode deserialize: {e}")))?;
        if snapshot.version != FORMAT_VERSION {
            return Err(std::io::Error::other(format!(
                "system include cache version mismatch: file={}, expected={}",
                snapshot.version, FORMAT_VERSION
            )));
        }

        let mut cache: HashMap<NormalizedPath, SystemIncludeEntry> =
            HashMap::with_capacity(snapshot.entries.len());
        let entry_count = snapshot.entries.len();
        for (key, value) in snapshot.entries {
            cache.insert(key, value);
        }
        tracing::info!(
            path = %path.display(),
            entries = entry_count,
            "system include cache restored from disk"
        );
        Ok(Self { cache })
    }
}

fn write_atomic_durable(tmp: &Path, target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    {
        let mut f = std::fs::File::create(tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(tmp, target)?;
    if let Some(parent) = target.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_gcc_output() {
        let output = r#"Using built-in specs.
COLLECT_GCC=g++
COLLECT_LTO_WRAPPER=/usr/libexec/gcc/x86_64-linux-gnu/12/lto-wrapper
#include "..." search starts here:
#include <...> search starts here:
 /usr/include/c++/12
 /usr/include/x86_64-linux-gnu/c++/12
 /usr/include/c++/12/backward
 /usr/lib/gcc/x86_64-linux-gnu/12/include
 /usr/local/include
 /usr/include/x86_64-linux-gnu
 /usr/include
End of search list.
"#;
        let paths = parse_system_include_output(output);
        assert_eq!(paths.len(), 7);
        assert_eq!(paths[0], NormalizedPath::new("/usr/include/c++/12"));
    }

    fn touch(path: &Path, content: &[u8]) {
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn cache_get_returns_paths_after_insert() {
        let tmp = tempfile::tempdir().unwrap();
        let compiler = tmp.path().join("clang++");
        touch(&compiler, b"#!/bin/sh\nexec /usr/bin/clang++ \"$@\"\n");

        let mut cache = SystemIncludeCache::new();
        cache.insert(NormalizedPath::new(&compiler), vec!["/usr/include".into()]);

        let got = cache.get(&compiler).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], NormalizedPath::new("/usr/include"));
    }

    #[test]
    fn cache_get_invalidates_on_binary_change() {
        // Issue #541: stat-verify must catch in-place compiler upgrades
        // (apt upgrade clang, brew upgrade, etc.) so the cached entry
        // doesn't outlive the binary that produced it.
        let tmp = tempfile::tempdir().unwrap();
        let compiler = tmp.path().join("clang++");
        touch(&compiler, b"#!/bin/sh\n# v1\n");
        filetime::set_file_mtime(
            &compiler,
            filetime::FileTime::from_unix_time(1_000_000_000, 0),
        )
        .unwrap();

        let mut cache = SystemIncludeCache::new();
        cache.insert(NormalizedPath::new(&compiler), vec!["/old/include".into()]);
        assert!(cache.get(&compiler).is_some(), "fresh insert must verify");

        // Simulate `apt upgrade clang` — same path, new binary.
        touch(
            &compiler,
            b"#!/bin/sh\n# v2 with a totally different size\n",
        );
        filetime::set_file_mtime(
            &compiler,
            filetime::FileTime::from_unix_time(1_000_001_000, 0),
        )
        .unwrap();

        assert!(
            cache.get(&compiler).is_none(),
            "binary change must invalidate the cached entry",
        );
    }

    #[test]
    fn cache_get_or_discover_caches() {
        let tmp = tempfile::tempdir().unwrap();
        let compiler = tmp.path().join("clang++");
        touch(&compiler, b"#!/bin/sh\n# v1\n");

        let mut cache = SystemIncludeCache::new();
        let discover_calls = std::cell::RefCell::new(0);

        let paths = cache.get_or_discover(&compiler, |_| {
            *discover_calls.borrow_mut() += 1;
            vec!["/usr/include".into()]
        });
        assert_eq!(paths.len(), 1);

        // Second call must NOT re-run discovery (entry is still fresh).
        let _ = cache.get_or_discover(&compiler, |_| {
            *discover_calls.borrow_mut() += 1;
            vec!["/should/not/happen".into()]
        });
        assert_eq!(*discover_calls.borrow(), 1);
    }

    #[test]
    fn cache_save_then_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let compiler = tmp.path().join("clang++");
        let snapshot = tmp.path().join("system_includes.bin");
        touch(&compiler, b"#!/bin/sh\nexec /usr/bin/clang++\n");

        let mut original = SystemIncludeCache::new();
        original.insert(
            NormalizedPath::new(&compiler),
            vec!["/usr/include".into(), "/usr/local/include".into()],
        );
        original.save_to_disk(&snapshot).unwrap();
        assert!(snapshot.exists(), "non-empty cache must produce a file");

        let restored = SystemIncludeCache::load_from_disk(&snapshot).unwrap();
        let got = restored.get(&compiler).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], NormalizedPath::new("/usr/include"));
        assert_eq!(got[1], NormalizedPath::new("/usr/local/include"));
    }

    #[test]
    fn cache_load_missing_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist.bin");
        let cache = SystemIncludeCache::load_from_disk(&missing).unwrap();
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn cache_save_empty_does_not_create_file() {
        let tmp = tempfile::tempdir().unwrap();
        let snapshot = tmp.path().join("system_includes.bin");
        let cache = SystemIncludeCache::new();
        cache.save_to_disk(&snapshot).unwrap();
        assert!(!snapshot.exists());
    }

    #[test]
    fn cache_load_rejects_version_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let snapshot = tmp.path().join("system_includes.bin");

        let bad = PersistedSystemIncludes {
            version: FORMAT_VERSION + 999,
            entries: Vec::new(),
        };
        let bytes = bincode::serialize(&bad).unwrap();
        std::fs::write(&snapshot, bytes).unwrap();

        let err = SystemIncludeCache::load_from_disk(&snapshot).unwrap_err();
        assert!(err.to_string().contains("version mismatch"));
    }

    #[test]
    fn cache_load_then_get_rehashes_when_binary_changes_after_save() {
        // Round-trip safety net: a stale on-disk snapshot must never
        // substitute for a real discovery after the compiler changed.
        let tmp = tempfile::tempdir().unwrap();
        let compiler = tmp.path().join("clang++");
        let snapshot = tmp.path().join("system_includes.bin");
        touch(&compiler, b"#!/bin/sh\n# original\n");
        filetime::set_file_mtime(
            &compiler,
            filetime::FileTime::from_unix_time(1_000_000_000, 0),
        )
        .unwrap();

        let mut original = SystemIncludeCache::new();
        original.insert(NormalizedPath::new(&compiler), vec!["/old/include".into()]);
        original.save_to_disk(&snapshot).unwrap();

        // Simulate compiler upgrade.
        touch(&compiler, b"#!/bin/sh\n# upgraded - different size\n");
        filetime::set_file_mtime(
            &compiler,
            filetime::FileTime::from_unix_time(1_000_001_000, 0),
        )
        .unwrap();

        let restored = SystemIncludeCache::load_from_disk(&snapshot).unwrap();
        assert!(
            restored.get(&compiler).is_none(),
            "stale snapshot must not survive a binary change",
        );
    }

    #[test]
    fn discovery_args_returns_nonempty() {
        assert!(!discovery_args().is_empty());
    }

    #[test]
    fn discovery_args_fast_contains_triple_hash() {
        // Sanity: the fast path uses `-###` so clang prints the cc1
        // command without spawning the real preprocessor.
        assert!(discovery_args_fast().contains(&"-###"));
        assert!(discovery_args_fast().contains(&"-E"));
        assert!(discovery_args_fast().contains(&"-x"));
        assert!(discovery_args_fast().contains(&"c++"));
    }

    #[test]
    fn parse_cc1_output_extracts_internal_isystem_paths() {
        // Realistic clang 18 -### output on Ubuntu 24.04. The cc1 line
        // is at the bottom, all args quoted with double quotes.
        let output = r#"clang version 18.1.3 (1ubuntu1)
Target: x86_64-pc-linux-gnu
Thread model: posix
InstalledDir: /usr/bin
Found candidate GCC installation: /usr/lib/gcc/x86_64-linux-gnu/13
Selected GCC installation: /usr/lib/gcc/x86_64-linux-gnu/13
Candidate multilib: .;@m64
Selected multilib: .;@m64
 "/usr/lib/llvm-18/bin/clang-18" "-cc1" "-triple" "x86_64-pc-linux-gnu" "-E" "-disable-free" "-disable-llvm-verifier" "-main-file-name" "null" "-mrelocation-model" "pic" "-pic-level" "2" "-mframe-pointer=all" "-fmath-errno" "-ffp-contract=on" "-fno-rounding-math" "-mconstructor-aliases" "-funwind-tables=2" "-target-cpu" "x86-64" "-debugger-tuning=gdb" "-fcoverage-compilation-dir=/tmp" "-resource-dir" "/usr/lib/llvm-18/lib/clang/18" "-internal-isystem" "/usr/bin/../lib/gcc/x86_64-linux-gnu/13/../../../../include/c++/13" "-internal-isystem" "/usr/bin/../lib/gcc/x86_64-linux-gnu/13/../../../../include/x86_64-linux-gnu/c++/13" "-internal-isystem" "/usr/bin/../lib/gcc/x86_64-linux-gnu/13/../../../../include/c++/13/backward" "-internal-isystem" "/usr/lib/llvm-18/lib/clang/18/include" "-internal-isystem" "/usr/local/include" "-internal-externc-isystem" "/usr/include/x86_64-linux-gnu" "-internal-externc-isystem" "/include" "-internal-externc-isystem" "/usr/include" "-fdeprecated-macro" "-ferror-limit" "19" "-fgnuc-version=4.2.1" "-fcxx-exceptions" "-fexceptions" "-fcolor-diagnostics" "-target-feature" "+cx8" "-target-feature" "+fxsr" "-target-feature" "+mmx" "-target-feature" "+sse" "-target-feature" "+sse2" "-target-feature" "+x87" "-faddrsig" "-D__GCC_HAVE_DWARF2_CFI_ASM=1" "-o" "-" "-x" "c++" "/dev/null"
"#;

        let paths = parse_cc1_system_include_output(output);
        assert_eq!(
            paths.len(),
            8,
            "expected 5 isystem + 3 externc-isystem entries, got {paths:?}",
        );
        // First captures must include the GCC C++ headers (driver
        // canonical-order):
        assert_eq!(
            paths[0],
            NormalizedPath::new(
                "/usr/bin/../lib/gcc/x86_64-linux-gnu/13/../../../../include/c++/13"
            ),
        );
        // Last must be /usr/include (the catch-all):
        assert_eq!(paths[7], NormalizedPath::new("/usr/include"));
    }

    #[test]
    fn parse_cc1_output_empty_when_no_cc1_line() {
        // Output without a cc1 line (e.g. gcc -### or a compiler that
        // doesn't support `-###`) must yield an empty path list so the
        // caller can fall back to the slow `-v -E` discovery.
        let output = "gcc version 13.2.0 (Ubuntu 13.2.0-23ubuntu4)\nCOLLECT_GCC_OPTIONS='-### -E -x c++' '-mtune=generic' '-march=x86-64'\n";
        let paths = parse_cc1_system_include_output(output);
        assert!(paths.is_empty(), "expected empty, got {paths:?}");
    }

    #[test]
    fn parse_cc1_output_handles_paths_with_spaces() {
        // Driver quoting preserves spaces inside paths — the tokenizer
        // must read the whole quoted segment as one token.
        let output =
            "\"/usr/bin/clang\" \"-cc1\" \"-E\" \"-internal-isystem\" \"/opt/Custom Include Path/v1\" \"-x\" \"c++\" \"/dev/null\"\n";
        let paths = parse_cc1_system_include_output(output);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0], NormalizedPath::new("/opt/Custom Include Path/v1"));
    }

    #[test]
    fn parse_cc1_output_skips_isystem_when_value_missing() {
        // Truncated cc1 line — the -internal-isystem flag with no value
        // following must not panic and must not produce an entry.
        let output = "\"/usr/bin/clang\" \"-cc1\" \"-internal-isystem\"\n";
        let paths = parse_cc1_system_include_output(output);
        assert!(paths.is_empty());
    }
}

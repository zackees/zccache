//! Cache-oriented commands: clear, kv, warm, crashes, cache-root, snapshot-bytes/fp.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::super::snapshot_fp;
use super::util::{format_bytes, LOST_CONNECTION_MSG};

pub(crate) async fn cmd_clear(endpoint: &str) -> ExitCode {
    let recv_result = match crate::ipc::daemon_control_roundtrip(
        endpoint,
        crate::ipc::DaemonControlRequest::Clear,
        None,
    )
    .await
    {
        Ok(response) => response,
        Err(e) if crate::cli::client::is_daemon_unreachable_err(&e) => {
            eprintln!("daemon not running at {endpoint} — nothing to clear");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("zccache[err][R]: broken connection to daemon: {e}");
            return ExitCode::FAILURE;
        }
    };

    match recv_result {
        Some(crate::protocol::Response::Cleared {
            artifacts_removed,
            metadata_cleared,
            dep_graph_contexts_cleared,
            on_disk_bytes_freed,
        }) => {
            println!("Cache cleared:");
            println!("  Artifacts removed:  {artifacts_removed}");
            println!("  Metadata cleared:   {metadata_cleared}");
            println!("  Dep graph contexts: {dep_graph_contexts_cleared}");
            if on_disk_bytes_freed > 0 {
                println!(
                    "  Disk freed:         {}",
                    format_bytes(on_disk_bytes_freed)
                );
            }
            ExitCode::SUCCESS
        }
        None => {
            eprintln!("{LOST_CONNECTION_MSG}");
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("zccache[err][U]: unexpected response from daemon: {other:?}");
            ExitCode::FAILURE
        }
    }
}

pub(crate) fn cmd_kv(action: super::args::KvCommands) -> ExitCode {
    use super::args::KvCommands;
    use crate::artifact::{Key, KvError, KvStore};
    use std::io::{Read, Write};

    fn open_store() -> Result<KvStore, ExitCode> {
        match KvStore::open_default() {
            Ok(s) => Ok(s),
            Err(e) => {
                eprintln!("zccache kv: open: {e}");
                Err(ExitCode::FAILURE)
            }
        }
    }

    fn parse_key(hex: &str) -> Result<Key, ExitCode> {
        Key::from_hex(hex).map_err(|e| {
            eprintln!("zccache kv: bad key: {e}");
            ExitCode::FAILURE
        })
    }

    match action {
        KvCommands::Get { namespace, hex_key } => {
            let store = match open_store() {
                Ok(s) => s,
                Err(c) => return c,
            };
            let key = match parse_key(&hex_key) {
                Ok(k) => k,
                Err(c) => return c,
            };
            match store.get(&namespace, &key) {
                Ok(Some(bytes)) => {
                    let stdout = std::io::stdout();
                    let mut handle = stdout.lock();
                    if let Err(e) = handle.write_all(&bytes) {
                        eprintln!("zccache kv get: write stdout: {e}");
                        return ExitCode::FAILURE;
                    }
                    ExitCode::SUCCESS
                }
                Ok(None) => ExitCode::from(2),
                Err(e) => {
                    eprintln!("zccache kv get: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        KvCommands::Put {
            namespace,
            hex_key,
            value_from,
            value_from_stdin,
        } => {
            let store = match open_store() {
                Ok(s) => s,
                Err(c) => return c,
            };
            let key = match parse_key(&hex_key) {
                Ok(k) => k,
                Err(c) => return c,
            };
            let bytes = if let Some(path) = value_from {
                match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("zccache kv put: read {path}: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            } else if value_from_stdin {
                let mut buf = Vec::new();
                if let Err(e) = std::io::stdin().read_to_end(&mut buf) {
                    eprintln!("zccache kv put: read stdin: {e}");
                    return ExitCode::FAILURE;
                }
                buf
            } else {
                eprintln!("zccache kv put: must specify --value-from <file> or --value-from-stdin");
                return ExitCode::FAILURE;
            };
            match store.put(&namespace, &key, &bytes) {
                Ok(_) => ExitCode::SUCCESS,
                Err(KvError::TooLarge(n, m)) => {
                    eprintln!("zccache kv put: value too large: {n} bytes (max {m})");
                    ExitCode::FAILURE
                }
                Err(e) => {
                    eprintln!("zccache kv put: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        KvCommands::Rm { namespace, hex_key } => {
            let store = match open_store() {
                Ok(s) => s,
                Err(c) => return c,
            };
            let key = match parse_key(&hex_key) {
                Ok(k) => k,
                Err(c) => return c,
            };
            match store.remove(&namespace, &key) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("zccache kv rm: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        KvCommands::Ls { namespace } => {
            let store = match open_store() {
                Ok(s) => s,
                Err(c) => return c,
            };
            match store.list_namespace(&namespace) {
                Ok(entries) => {
                    for (k, len) in entries {
                        println!("{}  {}", k.to_hex(), len);
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("zccache kv ls: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        KvCommands::Clear { namespace } => {
            let store = match open_store() {
                Ok(s) => s,
                Err(c) => return c,
            };
            match store.clear_namespace(&namespace) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("zccache kv clear: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        KvCommands::Stats => {
            let store = match open_store() {
                Ok(s) => s,
                Err(c) => return c,
            };
            let total = match store.total_bytes() {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("zccache kv stats: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let by_ns = match store.stats() {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("zccache kv stats: {e}");
                    return ExitCode::FAILURE;
                }
            };
            println!("total_bytes  {total}");
            for (ns, bytes) in by_ns {
                println!("{ns}  {bytes}");
            }
            ExitCode::SUCCESS
        }
    }
}

pub(crate) fn cmd_warm(target_dir: &Path, profile: &str) -> ExitCode {
    let cache_dir = crate::core::config::default_cache_dir();
    let index_path = crate::core::config::index_path_from_cache_dir(&cache_dir);
    let artifact_dir = cache_dir.join("artifacts");
    // Look for Cargo.lock in cwd or next to target_dir
    let lockfile = {
        let cwd = Path::new("Cargo.lock");
        let parent = target_dir.parent().map(|p| p.join("Cargo.lock"));
        if cwd.exists() {
            Some(cwd.to_path_buf())
        } else if let Some(ref p) = parent {
            if p.exists() {
                Some(p.clone())
            } else {
                None
            }
        } else {
            None
        }
    };
    match warm_target(
        index_path.as_ref(),
        artifact_dir.as_ref(),
        target_dir,
        profile,
        lockfile.as_deref(),
    ) {
        Ok((restored, skipped, errors)) => {
            println!("zccache warm: restored {restored} files, skipped {skipped}, errors {errors}");
            if errors > 0 {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("zccache warm: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Parse crate names from a Cargo.lock file.
/// Returns a set of crate names with hyphens converted to underscores
/// (matching how cargo names output files).
pub(crate) fn parse_lockfile_crates(
    lockfile: &Path,
) -> Result<std::collections::HashSet<String>, String> {
    let content = std::fs::read_to_string(lockfile)
        .map_err(|e| format!("failed to read {}: {e}", lockfile.display()))?;
    let mut crates = std::collections::HashSet::new();
    for line in content.lines() {
        // Cargo.lock format: name = "crate-name"
        if let Some(name) = line.strip_prefix("name = \"") {
            if let Some(name) = name.strip_suffix('"') {
                // Cargo converts hyphens to underscores in output filenames
                crates.insert(name.replace('-', "_"));
            }
        }
    }
    Ok(crates)
}

/// Check if an output filename matches any crate in the allowed set.
/// Output filenames look like: libserde-abc123.rlib, serde-abc123.d,
/// libproc_macro2-def456.so, etc.
pub(crate) fn artifact_matches_lockfile(
    filename: &str,
    allowed_crates: &std::collections::HashSet<String>,
) -> bool {
    // Strip lib prefix if present
    let name = filename.strip_prefix("lib").unwrap_or(filename);
    // Find the hash separator: last hyphen followed by hex chars
    // e.g., "serde-abc123.rlib" → crate name is "serde"
    // e.g., "proc_macro2-def456.rmeta" → crate name is "proc_macro2"
    // Walk from the end to find the hash suffix
    if let Some(pos) = name.rfind('-') {
        let crate_name = &name[..pos];
        allowed_crates.contains(crate_name)
    } else {
        // No hash separator — might be a build script or other file, allow it
        true
    }
}

/// Core logic for `zccache warm` — testable with custom paths.
/// If lockfile is Some, only restores artifacts matching crates in the lockfile.
pub(crate) fn warm_target(
    index_path: &Path,
    artifact_dir: &Path,
    target_dir: &Path,
    profile: &str,
    lockfile: Option<&Path>,
) -> Result<(u64, u64, u64), String> {
    if !index_path.exists() {
        return Err(format!("no artifact index at {}", index_path.display()));
    }

    let store = crate::artifact::ArtifactStore::open(index_path)
        .map_err(|e| format!("failed to open artifact index: {e}"))?;

    let all_entries = store.load_all();

    if all_entries.is_empty() {
        return Err("no cached artifacts found in index".to_string());
    }

    // If we have a lockfile, only restore artifacts matching its crates
    let allowed_crates = match lockfile {
        Some(lf) => Some(parse_lockfile_crates(lf)?),
        None => None,
    };

    let artifacts = all_entries;

    let deps_dir = target_dir.join(profile).join("deps");
    std::fs::create_dir_all(&deps_dir)
        .map_err(|e| format!("failed to create {}: {e}", deps_dir.display()))?;
    // mtime bump below is the LRU recency signal for zccache's *own*
    // artifact-cache eviction (see `crates/zccache-daemon/src/eviction.rs`,
    // which picks the highest mtime across an artifact group as last-use).
    // We hardlink each artifact-cache file into target/, which shares an
    // inode with the cache file — so touching the dst here also bumps the
    // cache file's mtime, telling eviction "this artifact was just used,
    // don't evict it". NOT a cargo-freshness signal: cargo never
    // mtime-checks rlib outputs (they're content-keyed by their filename
    // hash), so don't be tempted to remove this thinking it duplicates
    // snapshot-fp-validate. Doing so would silently regress eviction.
    let now = std::time::SystemTime::now();
    let file_times = std::fs::FileTimes::new()
        .set_accessed(now)
        .set_modified(now);

    // Flatten the artifact → output-name nesting into a single Vec of
    // (src, dst, name) so we can parallelize the per-file work below.
    // Each entry is independent: a hardlink + touch of one cache file
    // into one output path. CI cache restores can be 1k–5k entries, and
    // the per-file syscalls (remove_file + hard_link + open + set_times)
    // dominate; rayon takes us from ~100 µs/file serial to N_cores-way
    // parallel on warm OS cache.
    let total_outputs: usize = artifacts
        .iter()
        .map(|(_, idx)| idx.output_names.len())
        .sum();
    let mut work: Vec<(std::path::PathBuf, std::path::PathBuf, String)> =
        Vec::with_capacity(total_outputs);
    for (key_hex, idx) in &artifacts {
        for (i, name) in idx.output_names.iter().enumerate() {
            work.push((
                artifact_dir.join(format!("{key_hex}_{i}")),
                deps_dir.join(name.as_str()),
                name.clone(),
            ));
        }
    }

    use rayon::prelude::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    let restored = AtomicU64::new(0);
    let skipped = AtomicU64::new(0);
    let errors = AtomicU64::new(0);

    work.par_iter().for_each(|(src, dst, name)| {
        // Skip if artifact doesn't match any crate in the lockfile.
        if let Some(ref allowed) = allowed_crates {
            if !artifact_matches_lockfile(name, allowed) {
                skipped.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }

        // Skip if source payload does not exist on disk.
        if !src.exists() {
            skipped.fetch_add(1, Ordering::Relaxed);
            return;
        }

        // Remove existing file at destination (hardlink will fail if it exists).
        if dst.exists() {
            if let Err(e) = std::fs::remove_file(dst) {
                eprintln!(
                    "zccache warm: failed to remove existing {}: {e}",
                    dst.display()
                );
                errors.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }

        // Try hardlink first, fall back to copy.
        let linked = std::fs::hard_link(src, dst).is_ok();
        if !linked {
            if let Err(e) = std::fs::copy(src, dst) {
                eprintln!(
                    "zccache warm: failed to copy {} -> {}: {e}",
                    src.display(),
                    dst.display()
                );
                errors.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }

        // Touch the just-hardlinked dst to bump the underlying inode's
        // mtime, which propagates to the artifact-cache file via the
        // shared-inode hardlink. See the comment on `file_times` above
        // — this is the LRU recency signal for eviction, not a
        // cargo-freshness hack.
        if let Ok(f) = std::fs::File::open(dst) {
            let _ = f.set_times(file_times);
        }

        restored.fetch_add(1, Ordering::Relaxed);
    });

    Ok((
        restored.into_inner(),
        skipped.into_inner(),
        errors.into_inner(),
    ))
}

pub(crate) fn cmd_crashes(clear: bool) -> ExitCode {
    let crash_dir = crate::core::config::crash_dump_dir();

    if clear {
        let count = match std::fs::read_dir(&crash_dir) {
            Ok(entries) => {
                let mut n = 0u64;
                for entry in entries.flatten() {
                    if std::fs::remove_file(entry.path()).is_ok() {
                        n += 1;
                    }
                }
                n
            }
            Err(_) => 0,
        };
        println!("Deleted {count} crash dump(s).");
        return ExitCode::SUCCESS;
    }

    let mut dumps: Vec<_> = match std::fs::read_dir(&crash_dir) {
        Ok(entries) => entries
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "txt"))
            .collect(),
        Err(_) => {
            println!("No crash dumps found.");
            return ExitCode::SUCCESS;
        }
    };

    if dumps.is_empty() {
        println!("No crash dumps found.");
        return ExitCode::SUCCESS;
    }

    dumps.sort_by_key(|e| e.file_name());

    println!("Crash dumps ({}):", dumps.len());
    println!();
    for entry in &dumps {
        let path = entry.path();
        println!("  {}", path.display());
        if let Ok(content) = std::fs::read_to_string(&path) {
            for (i, line) in content.lines().enumerate() {
                if i >= 5 {
                    println!("    ...");
                    break;
                }
                println!("    {line}");
            }
            println!();
        }
    }

    ExitCode::SUCCESS
}

/// Print the resolved cache root and how it was determined. Issue #275:
/// soldr (and any other wrapper) calls this to confirm at runtime that the
/// zccache binary on PATH honored `ZCCACHE_CACHE_DIR` before trusting the
/// Defender-exclusion contract.
pub(crate) fn cmd_cache_root(json: bool) -> ExitCode {
    let (root, source) = crate::core::config::resolve_cache_root();
    if json {
        let payload = serde_json::json!({
            "cache_root": root.as_path(),
            "source": source.as_str(),
            "daemon_namespace": crate::core::config::daemon_namespace_label(),
            "daemon_endpoint": crate::ipc::default_endpoint(),
        });
        println!("{}", serde_json::to_string(&payload).unwrap_or_default());
    } else {
        println!("{}", root.display());
    }
    ExitCode::SUCCESS
}

/// `zccache release-handles --path X` (#694 Phase 2 / builds on #690).
///
/// Wraps the existing `Request::ReleaseWorktreeHandles` daemon-side handler
/// in a standalone CLI surface so direct-zccache consumers (FastLED CI
/// teardown, sysadmin scripts, anything that does `git worktree remove` or
/// `rmdir` before tearing down a path) can break the daemon's session
/// handles deterministically without going through a router or wrapper.
///
/// The daemon refuses to release handles whose target contains the cache
/// root (`handle_release_worktree_handles` returns `Response::Error`); the
/// CLI surfaces that as a non-zero exit and a stderr message.
pub(crate) async fn cmd_release_handles(endpoint: &str, path: PathBuf, json: bool) -> ExitCode {
    let recv_result =
        match crate::ipc::daemon_release_worktree_handles_roundtrip(endpoint, path.clone(), None)
            .await
        {
            Ok(response) => response,
            Err(e) if crate::cli::client::is_daemon_unreachable_err(&e) => {
                eprintln!(
                    "daemon not running at {endpoint} — nothing to release for {}",
                    path.display()
                );
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("zccache[err][R]: broken connection to daemon: {e}");
                return ExitCode::FAILURE;
            }
        };

    match recv_result {
        Some(crate::protocol::Response::ReleaseWorktreeHandlesResult {
            inspected,
            released,
            sessions_dropped,
            unreleased,
        }) => {
            if json {
                let payload = serde_json::json!({
                    "inspected": inspected,
                    "released": released,
                    "sessions_dropped": sessions_dropped,
                    "unreleased": unreleased.iter().map(|p| p.as_path()).collect::<Vec<_>>(),
                });
                println!("{}", serde_json::to_string(&payload).unwrap_or_default());
            } else {
                println!(
                    "released {released} of {inspected} session handle(s) under {}",
                    path.display()
                );
                if !sessions_dropped.is_empty() {
                    println!("  sessions dropped: {}", sessions_dropped.join(", "));
                }
                if !unreleased.is_empty() {
                    println!("  unreleased paths:");
                    for p in &unreleased {
                        println!("    {}", p.as_path().display());
                    }
                }
            }
            ExitCode::SUCCESS
        }
        Some(crate::protocol::Response::Error { message }) => {
            eprintln!("zccache release-handles: {message}");
            ExitCode::FAILURE
        }
        None => {
            eprintln!("{LOST_CONNECTION_MSG}");
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("zccache[err][U]: unexpected response from daemon: {other:?}");
            ExitCode::FAILURE
        }
    }
}

/// `zccache cache size` (#695 Phase 1). Walks the resolved cache root and
/// prints the total bytes of every regular file under it. Hardlinks are
/// counted once via `snapshot_bytes_walk`'s `(dev, inode)` dedup.
///
/// The walk reuses the same parallel jwalk helper as `snapshot-bytes`, so
/// it picks up the Windows-Defender mitigation already validated by #189.
/// `prune_incremental` / `prune_build_script_out` do not apply here — the
/// zccache cache root has no `target/incremental` / `target/build/*/out`
/// subtrees, so passing `false`/`false` walks every byte.
pub(crate) fn cmd_cache_size(json: bool) -> ExitCode {
    let (root, _) = crate::core::config::resolve_cache_root();
    let bytes = match snapshot_bytes_walk(root.as_path(), false, false) {
        Ok(total) => total,
        Err(err) => {
            eprintln!("zccache cache size: {err}");
            return ExitCode::FAILURE;
        }
    };
    let human = format_bytes(bytes);
    if json {
        let payload = serde_json::json!({
            "bytes": bytes,
            "human": human,
            "cache_root": root.as_path(),
        });
        println!("{}", serde_json::to_string(&payload).unwrap_or_default());
    } else {
        println!("{bytes}\t{human}\t{}", root.display());
    }
    ExitCode::SUCCESS
}

/// `zccache cache list` (#695 Phase 1). Enumerates per-version cache
/// directories visible under the resolved cache root and prints each with
/// its size, last-active time, and status.
///
/// **On today's single-root cache** (no router, no multi-version layout) this
/// prints exactly one row — the resolved root itself, status `current`.
///
/// **Once #694 lands** the multi-version `~/.zccache/v-<version>/` layout, this
/// same code path discovers each `v-*` subdirectory as its own row without
/// further CLI changes. Last-active comes from a `v-<version>/active.json`
/// metadata file (written by the per-version daemon on a throttle) when
/// present, falling back to the cache root mtime when it is not.
pub(crate) fn cmd_cache_list(json: bool) -> ExitCode {
    let (root, _) = crate::core::config::resolve_cache_root();
    let rows = collect_cache_list_rows(root.as_path());

    if json {
        let payload: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "version": r.version,
                    "status": r.status,
                    "size_bytes": r.size_bytes,
                    "last_active_unix": r.last_active_unix,
                    "path": r.path,
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&payload).unwrap_or_default());
        return ExitCode::SUCCESS;
    }

    if rows.is_empty() {
        println!("(no cache directories under {})", root.display());
        return ExitCode::SUCCESS;
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!(
        "{:<24}  {:<10}  {:>12}  {:<18}  path",
        "version", "status", "size", "last-active"
    );
    for r in &rows {
        let last_active = match r.last_active_unix {
            Some(ts) => format_relative(now, ts),
            None => "-".to_string(),
        };
        println!(
            "{:<24}  {:<10}  {:>12}  {:<18}  {}",
            r.version,
            r.status,
            format_bytes(r.size_bytes),
            last_active,
            r.path
        );
    }
    ExitCode::SUCCESS
}

/// One row of `zccache cache list` output.
#[derive(Debug, serde::Serialize)]
pub(crate) struct CacheListRow {
    pub version: String,
    pub status: &'static str,
    pub size_bytes: u64,
    pub last_active_unix: Option<u64>,
    pub path: String,
}

/// Build the rows for `cmd_cache_list`. Today only the resolved cache root
/// appears (labeled `current`); when `<root>/v-<version>/` directories exist
/// (router rollout from #694), each is enumerated separately.
fn collect_cache_list_rows(root: &Path) -> Vec<CacheListRow> {
    let mut rows: Vec<CacheListRow> = Vec::new();

    // 1. Multi-version directories: `<root>/v-<version>/`. Present after the
    //    router lands; absent today. The walk is intentionally tolerant of
    //    a missing root or a root without any `v-*` children.
    if let Ok(read_dir) = std::fs::read_dir(root) {
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("v-") {
                continue;
            }
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let version = name.strip_prefix("v-").unwrap_or(&name).to_string();
            let size_bytes = snapshot_bytes_walk(&path, false, false).unwrap_or(0);
            let last_active_unix = read_active_json(&path).or_else(|| dir_mtime_unix(&path));
            rows.push(CacheListRow {
                version,
                status: "warm",
                size_bytes,
                last_active_unix,
                path: path.display().to_string(),
            });
        }
    }

    // 2. The legacy single-root cache itself, labeled `current`. Skip when
    //    the root is unreadable (matches `cmd_cache_size` semantics).
    if root.exists() {
        let size_bytes = snapshot_bytes_walk(root, false, false).unwrap_or(0);
        let last_active_unix = read_active_json(root).or_else(|| dir_mtime_unix(root));
        rows.push(CacheListRow {
            version: "(current single-root)".to_string(),
            status: "current",
            size_bytes,
            last_active_unix,
            path: root.display().to_string(),
        });
    }

    rows
}

/// Read `<dir>/active.json` and return the `last_accepted_ms` field as
/// Unix seconds, or `None` if the file is missing / malformed. See the
/// `active.json` schema in #695.
fn read_active_json(dir: &Path) -> Option<u64> {
    let bytes = std::fs::read(dir.join("active.json")).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let ms = value.get("last_accepted_ms")?.as_u64()?;
    Some(ms / 1000)
}

/// Directory mtime as Unix seconds, or `None` on stat failure.
fn dir_mtime_unix(dir: &Path) -> Option<u64> {
    std::fs::metadata(dir)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Human-readable relative time ("12 min ago", "3 days ago"). Lazy to keep
/// out of the chrono dep tree; the rounding only needs to match the column
/// width.
fn format_relative(now: u64, then: u64) -> String {
    if then > now {
        return "in the future".to_string();
    }
    let secs = now.saturating_sub(then);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{} min ago", secs / 60)
    } else if secs < 86_400 {
        format!("{} hr ago", secs / 3600)
    } else {
        format!("{} days ago", secs / 86_400)
    }
}

/// Parallel walk of `target` summing the bytes of every regular file, with
/// optional pruning. Uses jwalk for parallel readdir + stat (rayon under the
/// hood) — on Windows this hides per-file Defender callback latency that
/// dominates the single-threaded `os.walk` baseline. See zccache#189.
pub(crate) fn cmd_snapshot_bytes(
    target: &Path,
    prune_incremental: bool,
    prune_build_script_out: bool,
) -> ExitCode {
    match snapshot_bytes_walk(target, prune_incremental, prune_build_script_out) {
        Ok(total) => {
            println!("{total}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("zccache snapshot-bytes: {err}");
            ExitCode::FAILURE
        }
    }
}

pub(crate) fn cmd_snapshot_fp_record(
    target_dir: &Path,
    workspace_root: Option<PathBuf>,
    profile: &str,
    manifest_path: Option<PathBuf>,
) -> ExitCode {
    let workspace = workspace_root.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
    let manifest = manifest_path.unwrap_or_else(|| target_dir.join(snapshot_fp::MANIFEST_FILENAME));
    match snapshot_fp::record(target_dir, &workspace, &manifest, profile) {
        Ok(stats) => {
            eprintln!(
                "zccache snapshot-fp-record: wrote {} ({} crates, {} sources)",
                manifest.display(),
                stats.crates_recorded,
                stats.sources_hashed,
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("zccache snapshot-fp-record: {e}");
            ExitCode::FAILURE
        }
    }
}

pub(crate) fn cmd_snapshot_fp_validate(
    target_dir: &Path,
    workspace_root: Option<PathBuf>,
    profile: &str,
    manifest_path: Option<PathBuf>,
    stamp_seconds_ahead: u64,
) -> ExitCode {
    let workspace = workspace_root.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
    let manifest = manifest_path.unwrap_or_else(|| target_dir.join(snapshot_fp::MANIFEST_FILENAME));
    match snapshot_fp::validate(
        target_dir,
        &workspace,
        &manifest,
        profile,
        stamp_seconds_ahead,
    ) {
        Ok(stats) => {
            eprintln!(
                "zccache snapshot-fp-validate: {} clean / {} dirty",
                stats.crates_clean,
                stats.crates_dirty(),
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("zccache snapshot-fp-validate: {e}");
            ExitCode::FAILURE
        }
    }
}

pub(crate) fn snapshot_bytes_walk(
    target: &Path,
    prune_incremental: bool,
    prune_build_script_out: bool,
) -> std::io::Result<u64> {
    use jwalk::WalkDirGeneric;
    use std::sync::Mutex;

    if !target.exists() {
        return Ok(0);
    }

    // Dedup by (dev, inode) so hardlinked files don't double-count.
    let seen: Mutex<std::collections::HashSet<(u64, u64)>> = Mutex::new(Default::default());

    let walker = WalkDirGeneric::<((), Option<u64>)>::new(target).process_read_dir(
        move |_depth, parent_path, _read_dir_state, children| {
            for child in children.iter_mut() {
                let Ok(entry) = child.as_mut() else { continue };
                if !entry.file_type().is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if prune_incremental && name == "incremental" {
                    entry.read_children_path = None;
                    continue;
                }
                if prune_build_script_out && name == "out" {
                    // `*/build/*/out` — only prune if grandparent is `build`.
                    if let Some(grandparent) = parent_path.parent() {
                        if grandparent.file_name().and_then(|s| s.to_str()) == Some("build") {
                            entry.read_children_path = None;
                        }
                    }
                }
            }
        },
    );

    let mut total: u64 = 0;
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                // Tolerate per-entry stat failures the same way `os.walk` does
                // in the bash fallback: skip and continue. We only bail on
                // catastrophic root-level failure (handled by walker init).
                eprintln!("zccache snapshot-bytes: skip entry: {err}");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if let Some(key) = file_identity(&meta) {
            let mut seen_guard = seen.lock().expect("seen mutex poisoned");
            if !seen_guard.insert(key) {
                continue;
            }
        }
        total = total.saturating_add(meta.len());
    }
    Ok(total)
}

#[cfg(unix)]
fn file_identity(meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}

#[cfg(windows)]
fn file_identity(_meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    // Windows file IDs require a separate Win32 call; not worth the cost just
    // for hardlink dedup in a target/ tree. Cargo doesn't hardlink within
    // `target/` in practice, so the dedup is a no-op here.
    None
}

#[cfg(not(any(unix, windows)))]
fn file_identity(_meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

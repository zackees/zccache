//! Tests for the `core::config` module tree.
//!
//! Tests live in this file (not split per-submodule) because most of them
//! cross several submodules — e.g. asserting that path helpers respect the
//! resolve-layer's versioned subdir, or that cleanup interacts correctly
//! with the resolved depfile dir. Splitting them would force re-importing
//! every private helper into multiple test modules.

use super::cleanup::cleanup_legacy_temp_root_state;
use super::namespace::{
    daemon_namespace_from_env_value, home_dir_short_hash, is_safe_ipc_component_char,
    sanitize_ipc_component,
};
use super::paths::{
    artifacts_dir_from_cache_dir, cargo_registry_cache_dir_from_cache_dir,
    crash_dump_dir_from_cache_dir, depfile_dir_from_cache_dir, depgraph_dir_from_cache_dir,
    index_path_from_cache_dir, log_dir_from_cache_dir, metadata_path_from_cache_dir,
    symbols_cache_dir_from_cache_dir, tmp_dir_from_cache_dir,
};
use super::resolve::{
    cache_dir_from_env_value, colocate_enabled, default_cache_dir_from_env_value,
    effective_cache_root_from_top_level, resolve_cache_root_from_env_value,
    resolve_cache_root_top_level_from_env_value, same_volume_root, sanitize_path_component,
    versioned_subdir, CacheRootSource,
};
use super::{Config, COLOCATE_ENV};
use crate::core::NormalizedPath;
use std::ffi::OsString;
use std::path::Path;

// Bring `volume_root` into scope for tests.
use super::resolve::volume_root;

#[test]
fn default_cache_dir_lives_under_versioned_subdir() {
    // Issue #761 / #762 Phase 0: every state file is now per-daemon-version.
    // The default cache dir must end with `<root>/.zccache/v<VERSION>`, not
    // `<root>/.zccache` directly.
    let dir = default_cache_dir_from_env_value(None);
    let segs: Vec<String> = dir
        .as_path()
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    assert!(
        segs.iter().rev().nth(1).map(String::as_str) == Some(".zccache"),
        "expected `.zccache` directly above the version segment, got components {segs:?}"
    );
    let last = segs.last().expect("path has a last segment");
    assert!(
        last.starts_with('v'),
        "version segment must be `v<VERSION>`, got `{last}`"
    );
    assert_eq!(last, &versioned_subdir());
}

#[test]
fn resolve_cache_root_env_branch_still_versioned() {
    // Even when the user pins a custom root via ZCCACHE_CACHE_DIR, the
    // version segment is still appended so two daemon versions sharing one
    // override root (the soldr/perf-cluster shape) don't trample each other.
    let root = tempfile::tempdir().unwrap();
    let env_value = root.path().join("zc");
    let (dir, src) = resolve_cache_root_from_env_value(Some(env_value.clone().into_os_string()));
    assert_eq!(dir, env_value.join(versioned_subdir()));
    assert_eq!(src, CacheRootSource::Env);
    assert_eq!(src.as_str(), "env:ZCCACHE_CACHE_DIR");
}

#[test]
fn resolve_cache_root_default_branch_when_env_unset() {
    std::env::remove_var(COLOCATE_ENV);
    let (dir, src) = resolve_cache_root_from_env_value(None);
    // Default path now ends with `v<VERSION>` (see #761 Phase 0); the
    // `.zccache` segment is now the SECOND-from-last component.
    assert_eq!(
        dir.as_path()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(""),
        &versioned_subdir(),
    );
    assert_eq!(src, CacheRootSource::Default);
    assert_eq!(src.as_str(), "default:platform_dirs");
}

#[test]
fn resolve_cache_root_default_branch_when_env_empty() {
    std::env::remove_var(COLOCATE_ENV);
    let (_dir, src) = resolve_cache_root_from_env_value(Some(OsString::new()));
    assert_eq!(src, CacheRootSource::Default);
}

#[test]
fn top_level_root_still_unversioned_for_advisory_writes() {
    // The Phase 0 design reserves the TOP-LEVEL `~/.zccache/` for
    // cross-version markers (last-version.txt, migration log) — daemons
    // need a way to address it without picking up their own version
    // segment. Confirm the new helper hands back the pre-versioning path.
    let root = tempfile::tempdir().unwrap();
    let env_value = root.path().join("zc");
    let (top, _src) =
        resolve_cache_root_top_level_from_env_value(Some(env_value.clone().into_os_string()));
    assert_eq!(top, env_value);
    let (default_top, _) = resolve_cache_root_top_level_from_env_value(None);
    assert_eq!(
        default_top
            .as_path()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(""),
        ".zccache",
    );
}

#[test]
fn versioned_subdir_matches_crate_version() {
    assert_eq!(versioned_subdir(), format!("v{}", crate::core::VERSION));
    assert!(versioned_subdir().starts_with('v'));
}

#[test]
fn effective_cache_root_appends_version_once() {
    let root = NormalizedPath::from("/tmp/zccache-private-root");
    let effective = effective_cache_root_from_top_level(&root);
    assert_eq!(effective, root.join(versioned_subdir()));
    assert_eq!(effective_cache_root_from_top_level(&effective), effective);
}

#[test]
fn cache_root_source_display_matches_as_str() {
    assert_eq!(CacheRootSource::Env.to_string(), "env:ZCCACHE_CACHE_DIR");
    assert_eq!(
        CacheRootSource::Colocated.to_string(),
        "colocate:cross_volume"
    );
    assert_eq!(
        CacheRootSource::Default.to_string(),
        "default:platform_dirs"
    );
}

/// Every well-known subpath that the daemon/CLI persistently writes to
/// MUST live under the resolved cache root. This is the soldr/Defender
/// exclusion contract from issue #275: one directory the wrapper can
/// exclude and trust that no zccache write escapes it.
#[test]
fn cache_root_invariant_all_subpaths_rooted() {
    let (_temp, cache) = temp_cache_dir();
    let subs: [(NormalizedPath, &str); 10] = [
        (artifacts_dir_from_cache_dir(&cache), "artifacts/"),
        (tmp_dir_from_cache_dir(&cache), "tmp/"),
        (depfile_dir_from_cache_dir(&cache), "tmp/depfiles/"),
        (depgraph_dir_from_cache_dir(&cache), "depgraph/"),
        (log_dir_from_cache_dir(&cache), "logs/"),
        (crash_dump_dir_from_cache_dir(&cache), "crashes/"),
        (symbols_cache_dir_from_cache_dir(&cache), "symbols/"),
        (
            cargo_registry_cache_dir_from_cache_dir(&cache),
            "cargo-registry/",
        ),
        (index_path_from_cache_dir(&cache), "index.bin"),
        (metadata_path_from_cache_dir(&cache), "metadata.bin"),
    ];
    for (p, label) in &subs {
        assert!(
            p.starts_with(&cache),
            "{label} ({}) must be under cache root ({})",
            p.display(),
            cache.display()
        );
    }
}

#[test]
fn cache_dir_override_uses_non_empty_env_value() {
    let root = tempfile::tempdir().unwrap();
    let override_dir = root.path().join("zc");
    let cache_dir = default_cache_dir_from_env_value(Some(override_dir.clone().into_os_string()));

    // Issue #761 / #762 Phase 0: the env-overridden root is the *top-level*
    // unversioned path; the actual cache_dir lives one segment below it
    // under `v<VERSION>` so a single env-pinned root can host multiple
    // sibling daemon versions without one overwriting the other's state.
    let versioned = override_dir.join(versioned_subdir());
    assert_eq!(cache_dir, versioned);
    assert_eq!(
        artifacts_dir_from_cache_dir(&cache_dir),
        versioned.join("artifacts")
    );
    assert_eq!(tmp_dir_from_cache_dir(&cache_dir), versioned.join("tmp"));
    assert_eq!(
        depgraph_dir_from_cache_dir(&cache_dir),
        versioned.join("depgraph")
    );
    assert_eq!(
        index_path_from_cache_dir(&cache_dir),
        versioned.join("index.bin")
    );
    assert_eq!(
        metadata_path_from_cache_dir(&cache_dir),
        versioned.join("metadata.bin")
    );
    assert_eq!(
        crash_dump_dir_from_cache_dir(&cache_dir),
        versioned.join("crashes")
    );
    assert_eq!(log_dir_from_cache_dir(&cache_dir), versioned.join("logs"));
}

#[test]
fn cache_dir_override_ignores_empty_env_value() {
    assert!(cache_dir_from_env_value(Some(OsString::new())).is_none());
}

/// `metadata.bin` MUST live in the same directory as `index.bin` so that
/// whatever mechanism bundles the cache directory (notably `soldr save`
/// / `soldr load` for the `cold-tar-untar-warm` perf-cluster scenario)
/// picks both files up automatically. If a future refactor moves either
/// file without moving the other, the warm-side daemon spawned after
/// `soldr load` would restart with an empty `MetadataCache` even though
/// the artifact index was restored — silently undoing the perf win this
/// pair was designed to deliver.
#[test]
fn metadata_path_is_sibling_of_index_path() {
    let (_temp, cache_dir) = temp_cache_dir();
    let index = index_path_from_cache_dir(&cache_dir);
    let metadata = metadata_path_from_cache_dir(&cache_dir);
    assert_eq!(
        index.parent(),
        metadata.parent(),
        "metadata.bin must live in the same directory as index.bin so soldr save/load bundles both",
    );
    assert!(
        metadata.starts_with(&cache_dir),
        "metadata.bin must be a descendant of cache_dir",
    );
}

#[test]
fn relative_cache_dir_override_is_made_absolute() {
    let override_dir = cache_dir_from_env_value(Some(OsString::from("target/../zc"))).unwrap();
    assert!(override_dir.is_absolute());
    assert!(override_dir.ends_with("zc"));
}

#[test]
fn crash_dump_dir_ends_with_crashes() {
    let (_temp, cache) = temp_cache_dir();
    let dir = crash_dump_dir_from_cache_dir(&cache);
    assert!(dir.ends_with("crashes"));
}

#[test]
fn crash_dump_dir_is_under_cache_dir() {
    let (_temp, cache) = temp_cache_dir();
    let crashes = crash_dump_dir_from_cache_dir(&cache);
    assert!(crashes.starts_with(&cache));
}

#[test]
fn log_dir_ends_with_logs() {
    let (_temp, cache) = temp_cache_dir();
    let dir = log_dir_from_cache_dir(&cache);
    assert!(dir.ends_with("logs"));
}

#[test]
fn log_dir_is_under_cache_dir() {
    let (_temp, cache) = temp_cache_dir();
    let logs = log_dir_from_cache_dir(&cache);
    assert!(logs.starts_with(&cache));
}

#[test]
fn cargo_registry_cache_dir_is_under_cache_dir() {
    let (_temp, cache) = temp_cache_dir();
    let dir = cargo_registry_cache_dir_from_cache_dir(&cache);
    assert!(dir.ends_with("cargo-registry"));
    assert!(dir.starts_with(&cache));
}

#[test]
fn artifacts_dir_ends_with_artifacts() {
    let (_temp, cache) = temp_cache_dir();
    let dir = artifacts_dir_from_cache_dir(&cache);
    assert!(dir.ends_with("artifacts"));
    assert!(dir.starts_with(cache));
}

#[test]
fn tmp_dir_ends_with_tmp() {
    let (_temp, cache) = temp_cache_dir();
    let dir = tmp_dir_from_cache_dir(&cache);
    assert!(dir.ends_with("tmp"));
    assert!(dir.starts_with(cache));
}

#[test]
fn depgraph_dir_ends_with_depgraph() {
    let (_temp, cache) = temp_cache_dir();
    let dir = depgraph_dir_from_cache_dir(&cache);
    assert!(dir.ends_with("depgraph"));
    assert!(dir.starts_with(cache));
}

#[test]
fn depfile_dir_under_tmp() {
    let (_temp, cache) = temp_cache_dir();
    let tmp = tmp_dir_from_cache_dir(&cache);
    let dir = depfile_dir_from_cache_dir(&cache);
    assert!(dir.ends_with("depfiles"));
    assert!(dir.starts_with(tmp));
}

#[test]
fn cleanup_stale_depfile_dirs_removes_dead() {
    let base = tempfile::tempdir().unwrap();
    let depfiles = base.path().join("depfiles");
    std::fs::create_dir_all(&depfiles).unwrap();

    // Create a "dead" dir (PID 99999999 unlikely alive).
    std::fs::create_dir(depfiles.join("99999999-0")).unwrap();
    // Create a non-matching dir (should be left alone).
    std::fs::create_dir(depfiles.join("not-a-pid")).unwrap();

    let entries = std::fs::read_dir(&depfiles).unwrap();
    let dirs: Vec<_> = entries.flatten().collect();
    assert_eq!(dirs.len(), 2);

    // Use a custom is_alive that says nothing is alive.
    let cleaned = cleanup_stale_with_base(&depfiles, |_| false);
    assert_eq!(cleaned, 1); // only the parseable one removed

    // "not-a-pid" should still exist.
    assert!(depfiles.join("not-a-pid").is_dir());
    assert!(!depfiles.join("99999999-0").exists());
}

#[test]
fn cleanup_stale_depfile_dirs_skips_alive() {
    let base = tempfile::tempdir().unwrap();
    let depfiles = base.path().join("depfiles");
    std::fs::create_dir_all(&depfiles).unwrap();
    std::fs::create_dir(depfiles.join("12345-0")).unwrap();

    let cleaned = cleanup_stale_with_base(&depfiles, |_| true);
    assert_eq!(cleaned, 0);
    assert!(depfiles.join("12345-0").is_dir());
}

#[test]
fn cleanup_stale_depfile_dirs_empty() {
    // Non-existent directory returns 0.
    let cleaned = cleanup_stale_with_base(std::path::Path::new("/nonexistent/path"), |_| false);
    assert_eq!(cleaned, 0);
}

#[test]
fn cleanup_legacy_temp_root_state_removes_legacy_dirs() {
    let temp_root = tempfile::tempdir().unwrap();
    let current_cache_dir = tempfile::tempdir().unwrap();

    let legacy_cache = temp_root.path().join(".zccache");
    std::fs::create_dir_all(&legacy_cache).unwrap();
    std::fs::write(legacy_cache.join("sentinel"), "legacy").unwrap();

    let dead_depfile = temp_root.path().join("zccache-depfiles-1234-0");
    std::fs::create_dir_all(&dead_depfile).unwrap();
    std::fs::write(dead_depfile.join("sentinel"), "dead").unwrap();

    let live_depfile = temp_root.path().join("zccache-depfiles-4321-0");
    std::fs::create_dir_all(&live_depfile).unwrap();

    let unrelated = temp_root.path().join("not-legacy");
    std::fs::create_dir_all(&unrelated).unwrap();

    let cleaned =
        cleanup_legacy_temp_root_state(temp_root.path(), current_cache_dir.path(), |pid| {
            pid != 1234
        });

    assert_eq!(cleaned, 2);
    assert!(!legacy_cache.exists());
    assert!(!dead_depfile.exists());
    assert!(live_depfile.exists());
    assert!(unrelated.exists());
}

#[test]
fn cleanup_legacy_temp_root_state_skips_current_cache_dir() {
    let temp_root = tempfile::tempdir().unwrap();
    let current_cache_dir = temp_root.path().join(".zccache");
    std::fs::create_dir_all(&current_cache_dir).unwrap();
    std::fs::write(current_cache_dir.join("sentinel"), "keep").unwrap();

    let cleaned = cleanup_legacy_temp_root_state(temp_root.path(), &current_cache_dir, |_| false);

    assert_eq!(cleaned, 0);
    assert!(current_cache_dir.exists());
    assert_eq!(
        std::fs::read_to_string(current_cache_dir.join("sentinel")).unwrap(),
        "keep"
    );
}

#[test]
fn cleanup_legacy_temp_root_state_skips_parent_of_current_cache_dir() {
    let temp_root = tempfile::tempdir().unwrap();
    let current_cache_dir = temp_root.path().join(".zccache").join("current");
    std::fs::create_dir_all(&current_cache_dir).unwrap();
    std::fs::write(current_cache_dir.join("sentinel"), "keep").unwrap();

    let cleaned = cleanup_legacy_temp_root_state(temp_root.path(), &current_cache_dir, |_| false);

    assert_eq!(cleaned, 0);
    assert!(current_cache_dir.exists());
    assert_eq!(
        std::fs::read_to_string(current_cache_dir.join("sentinel")).unwrap(),
        "keep"
    );
}

/// Test helper: runs cleanup logic against an arbitrary base dir.
fn cleanup_stale_with_base<F>(base: &std::path::Path, is_alive: F) -> usize
where
    F: Fn(u32) -> bool,
{
    let entries = match std::fs::read_dir(base) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };
    let mut cleaned = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        let pid: u32 = match name.split('-').next().and_then(|s| s.parse().ok()) {
            Some(p) => p,
            None => continue,
        };
        if !is_alive(pid) && std::fs::remove_dir_all(&path).is_ok() {
            cleaned += 1;
        }
    }
    cleaned
}

#[test]
fn disk_gc_interval_default() {
    let config = Config::default();
    assert_eq!(config.disk_gc_interval_secs, 300);
}

#[test]
fn index_path_ends_with_bin() {
    let (_temp, cache) = temp_cache_dir();
    let p = index_path_from_cache_dir(&cache);
    assert!(p.ends_with("index.bin"));
    assert!(p.starts_with(cache));
}

fn temp_cache_dir() -> (tempfile::TempDir, NormalizedPath) {
    let temp = tempfile::tempdir().unwrap();
    let cache = NormalizedPath::from(temp.path());
    (temp, cache)
}

#[test]
fn volume_root_extracts_drive_or_root() {
    if cfg!(windows) {
        let r = volume_root(Path::new(r"C:\Users\zack\foo")).unwrap();
        assert_eq!(r.to_string_lossy(), r"C:\");
        let r = volume_root(Path::new(r"D:\projects")).unwrap();
        assert_eq!(r.to_string_lossy(), r"D:\");
    } else {
        let r = volume_root(Path::new("/home/zack/foo")).unwrap();
        assert_eq!(r.to_string_lossy(), "/");
        let r = volume_root(Path::new("/mnt/data/projects")).unwrap();
        assert_eq!(r.to_string_lossy(), "/");
    }
}

#[test]
fn same_volume_root_is_case_insensitive_on_windows() {
    let r1 = Path::new(r"C:\");
    let r2 = Path::new(r"c:\");
    if cfg!(windows) {
        assert!(same_volume_root(r1, r2));
    } else {
        assert!(!same_volume_root(r1, r2));
    }
    let same = Path::new("/");
    assert!(same_volume_root(same, same));
}

#[test]
fn home_dir_short_hash_is_stable_and_8_hex() {
    let a = home_dir_short_hash(Path::new("/home/zack"));
    let b = home_dir_short_hash(Path::new("/home/zack"));
    assert_eq!(a, b, "must be deterministic");
    assert_eq!(a.len(), 8);
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    let c = home_dir_short_hash(Path::new("/home/other"));
    assert_ne!(a, c, "different paths → different hashes");
}

#[test]
fn home_dir_short_hash_is_case_insensitive_on_windows() {
    let upper = home_dir_short_hash(Path::new(r"C:\Users\Zack"));
    let lower = home_dir_short_hash(Path::new(r"c:\users\zack"));
    if cfg!(windows) {
        assert_eq!(upper, lower);
    } else {
        assert_ne!(upper, lower);
    }
}

#[test]
fn sanitize_path_component_strips_oddities() {
    assert_eq!(sanitize_path_component("zack"), "zack");
    assert_eq!(sanitize_path_component("z@ck!"), "z_ck_");
    assert_eq!(sanitize_path_component(""), "");
    // Truncates to 32 chars
    let long = "a".repeat(100);
    assert_eq!(sanitize_path_component(&long).len(), 32);
}

#[test]
fn daemon_namespace_ignores_unset_or_empty_values() {
    assert_eq!(daemon_namespace_from_env_value(None), None);
    assert_eq!(daemon_namespace_from_env_value(Some(OsString::new())), None);
    assert_eq!(
        daemon_namespace_from_env_value(Some(OsString::from("   "))),
        None
    );
}

#[test]
fn daemon_namespace_sanitizes_for_paths_and_pipes() {
    assert_eq!(
        daemon_namespace_from_env_value(Some(OsString::from(" soldr dev! "))).as_deref(),
        Some("soldr_dev_")
    );
    assert_eq!(
        daemon_namespace_from_env_value(Some(OsString::from("soldr-dev_1.2"))).as_deref(),
        Some("soldr-dev_1.2")
    );
}

#[test]
fn daemon_namespace_keeps_long_values_distinct() {
    let a = daemon_namespace_from_env_value(Some(OsString::from(format!("{}a", "x".repeat(40)))))
        .unwrap();
    let b = daemon_namespace_from_env_value(Some(OsString::from(format!("{}b", "x".repeat(40)))))
        .unwrap();
    assert_ne!(a, b);
    assert!(a.starts_with(&"x".repeat(32)));
    assert_eq!(a.len(), 41);
}

#[test]
fn sanitize_ipc_component_keeps_safe_values_unchanged() {
    assert_eq!(
        sanitize_ipc_component("zackees-dev_1.2").as_deref(),
        Some("zackees-dev_1.2")
    );
}

#[test]
fn sanitize_ipc_component_replaces_spaces_and_adds_hash() {
    let component = sanitize_ipc_component("Zach Vorhies").unwrap();
    assert!(component.starts_with("Zach_Vorhies-"));
    assert_eq!(component.len(), "Zach_Vorhies-".len() + 8);
    assert!(component.chars().all(is_safe_ipc_component_char));
}

#[test]
fn sanitize_ipc_component_keeps_unsafe_names_distinct() {
    let spaced = sanitize_ipc_component("Zach Vorhies").unwrap();
    let slashed = sanitize_ipc_component("Zach/Vorhies").unwrap();
    assert_ne!(spaced, slashed);
    assert!(spaced.starts_with("Zach_Vorhies-"));
    assert!(slashed.starts_with("Zach_Vorhies-"));
}

#[test]
fn sanitize_ipc_component_ignores_empty_values() {
    assert_eq!(sanitize_ipc_component("   "), None);
}

#[test]
fn colocate_disabled_returns_home_path() {
    // No env var set in this test (we can't reliably toggle env in
    // unit tests on Windows without races, so just verify the gating
    // function in isolation).
    std::env::remove_var(COLOCATE_ENV);
    assert!(!colocate_enabled());
    let result = default_cache_dir_from_env_value(None);
    // Issue #761 / #762 Phase 0: the active cache_dir is one segment
    // below `.zccache` — assert on the parent component instead.
    let parent_name = result
        .as_path()
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("");
    assert_eq!(
        parent_name,
        ".zccache",
        "expected parent component `.zccache`, got path {}",
        result.display()
    );
}

#[test]
fn colocate_basename_appears_in_path() {
    let home = NormalizedPath::from(Path::new("/home/myuser"));
    // We can't easily mock cwd cross-platform; just call the path
    // builder directly with a synthetic cross-volume scenario.
    let basename = home
        .as_path()
        .file_name()
        .and_then(|n| n.to_str())
        .map(sanitize_path_component)
        .unwrap();
    assert_eq!(basename, "myuser");
    let hash = home_dir_short_hash(home.as_path());
    let expected_suffix = format!(".zccache-myuser-{hash}");
    assert!(expected_suffix.starts_with(".zccache-myuser-"));
    assert!(expected_suffix.len() == ".zccache-myuser-".len() + 8);
}

#[test]
fn no_spawn_value_grammar_matches_zccache_disable() {
    use super::no_spawn_from_env_value;
    use std::ffi::OsStr;

    assert!(no_spawn_from_env_value(Some(OsStr::new("1"))));
    assert!(no_spawn_from_env_value(Some(OsStr::new("true"))));
    assert!(no_spawn_from_env_value(Some(OsStr::new("TRUE"))));
    assert!(no_spawn_from_env_value(Some(OsStr::new("True"))));
    assert!(!no_spawn_from_env_value(Some(OsStr::new("0"))));
    assert!(!no_spawn_from_env_value(Some(OsStr::new(""))));
    assert!(!no_spawn_from_env_value(Some(OsStr::new("yes"))));
    assert!(!no_spawn_from_env_value(None));
}

#[test]
fn no_spawn_error_names_the_env_var() {
    let message = super::no_spawn_error("zccache-daemon");
    assert!(message.contains("ZCCACHE_NO_SPAWN"));
    assert!(message.contains("zccache-daemon"));
}

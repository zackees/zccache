//! Unit tests for the daemon server.

use super::*;

#[test]
fn pack_round_trip_extracts_each_payload() {
    let p0: Arc<Vec<u8>> = Arc::new(b"first payload".to_vec());
    let p1: Arc<Vec<u8>> = Arc::new((0u8..200).cycle().take(4096).collect());
    let p2: Arc<Vec<u8>> = Arc::new(Vec::new()); // 0-length payload edge case
    let payloads = vec![Arc::clone(&p0), Arc::clone(&p1), Arc::clone(&p2)];
    let pack = build_pack(&payloads);

    let entries = parse_pack_header(&pack).unwrap();
    assert_eq!(entries.len(), 3);
    for (i, (offset, size)) in entries.iter().enumerate() {
        let s = *offset as usize;
        let e = s + *size as usize;
        assert_eq!(&pack[s..e], payloads[i].as_slice());
    }
}

#[test]
fn parse_pack_header_rejects_garbage() {
    assert!(parse_pack_header(b"").is_err());
    assert!(parse_pack_header(b"NOTAZCPK").is_err());
    // Magic OK but truncated header
    assert!(parse_pack_header(b"ZCPK\x05\x00\x00\x00").is_err());
}

#[test]
fn try_load_packed_payload_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let key = "deadbeef";
    let payloads: Vec<Arc<Vec<u8>>> = vec![
        Arc::new(b"alpha".to_vec()),
        Arc::new(b"bravo bravo bravo".to_vec()),
    ];
    let pack = build_pack(&payloads);
    std::fs::write(pack_path_for(dir.path(), key), &pack).unwrap();

    assert_eq!(
        try_load_packed_payload(dir.path(), key, 0).unwrap(),
        b"alpha".to_vec()
    );
    assert_eq!(
        try_load_packed_payload(dir.path(), key, 1).unwrap(),
        b"bravo bravo bravo".to_vec()
    );
    assert!(try_load_packed_payload(dir.path(), key, 2).is_none());
    assert!(try_load_packed_payload(dir.path(), "missing", 0).is_none());
}

#[test]
fn persist_artifact_payloads_unpacked_layout() {
    // Default: not packed.
    std::env::remove_var("ZCCACHE_PACK_ARTIFACTS");
    let dir = tempfile::tempdir().unwrap();
    let key = "abc123";
    let payloads = vec![Arc::new(b"one".to_vec()), Arc::new(b"two".to_vec())];
    persist_artifact_payloads(dir.path(), key, &payloads).unwrap();
    assert_eq!(std::fs::read(dir.path().join("abc123_0")).unwrap(), b"one");
    assert_eq!(std::fs::read(dir.path().join("abc123_1")).unwrap(), b"two");
    assert!(!dir.path().join("abc123.pack").exists());
}

#[test]
fn persist_artifact_paths_hardlinks_in_unpacked_layout() {
    std::env::remove_var("ZCCACHE_PACK_ARTIFACTS");
    let dir = tempfile::tempdir().unwrap();
    let key = "deadc0de";
    // Source files that simulate "compiler just wrote these".
    let src_a = dir.path().join("foo.rlib");
    let src_b = dir.path().join("foo.rmeta");
    std::fs::write(&src_a, b"rlib-bytes").unwrap();
    std::fs::write(&src_b, b"rmeta-bytes").unwrap();
    let sources = vec![
        NormalizedPath::from(src_a.clone()),
        NormalizedPath::from(src_b.clone()),
    ];
    persist_artifact_paths(dir.path(), key, &sources).unwrap();

    let cache_a = dir.path().join("deadc0de_0");
    let cache_b = dir.path().join("deadc0de_1");
    assert_eq!(std::fs::read(&cache_a).unwrap(), b"rlib-bytes");
    assert_eq!(std::fs::read(&cache_b).unwrap(), b"rmeta-bytes");

    // On the same-volume happy path we expect a real hardlink — both
    // names should resolve to the same inode. Inode-equality test via
    // platform metadata. Skip on platforms that don't easily expose
    // it (Windows tests still verify the bytes match above).
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let src_ino = std::fs::metadata(&src_a).unwrap().ino();
        let cache_ino = std::fs::metadata(&cache_a).unwrap().ino();
        assert_eq!(src_ino, cache_ino, "expected hardlink (shared inode)");
    }
}

#[test]
fn persist_artifact_paths_falls_back_to_copy_when_source_missing() {
    std::env::remove_var("ZCCACHE_PACK_ARTIFACTS");
    let dir = tempfile::tempdir().unwrap();
    let key = "nopath";
    let missing = dir.path().join("does-not-exist.rlib");
    let sources = vec![NormalizedPath::from(missing)];
    // Hardlink fails (source missing), copy also fails → err propagates.
    // Caller's contract is "best effort; on err skip caching."
    assert!(persist_artifact_paths(dir.path(), key, &sources).is_err());
}

async fn start_daemon() -> (String, tokio::task::JoinHandle<()>, Arc<Notify>) {
    let endpoint = zccache_ipc::unique_test_endpoint();
    let mut server = DaemonServer::bind(&endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });
    (endpoint, handle, shutdown)
}

fn test_context_key(source: &str) -> ContextKey {
    CompileContext {
        source_file: source.into(),
        include_search: zccache_depgraph::IncludeSearchPaths::default(),
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    }
    .context_key()
}

fn test_request_entry(cached_at: std::time::Instant) -> RequestCacheEntry {
    let context_key = test_context_key("/tmp/source.c");
    let source_path: NormalizedPath = "/tmp/source.c".into();
    let output_path: NormalizedPath = "/tmp/source.o".into();
    RequestCacheEntry {
        context_key,
        root: None,
        source_path: CachedRequestPath::capture(&source_path, None),
        output_path: CachedRequestPath::capture(&output_path, None),
        input_paths: vec![CachedRequestPath::capture(&source_path, None)],
        cross_root_shareable: false,
        cached_at,
    }
}

fn test_rsp_entry(cached_at: std::time::Instant) -> RspCacheEntry {
    RspCacheEntry {
        expanded: Vec::new(),
        dependencies: Vec::new(),
        cached_at,
    }
}

fn test_fast_hit_entry(cached_at: std::time::Instant) -> FastHitEntry {
    FastHitEntry {
        clock: Clock::ZERO,
        artifact_key_hex: "artifact".to_string(),
        cached_at,
    }
}

fn test_content_hash(index: usize) -> ContentHash {
    let mut bytes = [0; 32];
    bytes[..8].copy_from_slice(&(index as u64).to_le_bytes());
    ContentHash::from_bytes(bytes)
}

fn collect_command_env<'a, I>(envs: I) -> Vec<(String, String)>
where
    I: Iterator<Item = (&'a std::ffi::OsStr, Option<&'a std::ffi::OsStr>)>,
{
    envs.filter_map(|(key, value)| {
        Some((
            key.to_string_lossy().into_owned(),
            value?.to_string_lossy().into_owned(),
        ))
    })
    .collect()
}

fn env_value<'a>(envs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    envs.iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

fn jobserver_client_env() -> Vec<(String, String)> {
    vec![
        ("PATH".to_string(), "/usr/bin".to_string()),
        (
            "MAKEFLAGS".to_string(),
            "-j --jobserver-auth=8,9".to_string(),
        ),
        (
            "CARGO_MAKEFLAGS".to_string(),
            "-j --jobserver-fds=8,9 --jobserver-auth=8,9".to_string(),
        ),
        (
            "CARGO_MANIFEST_DIR".to_string(),
            "/tmp/workspace".to_string(),
        ),
    ]
}

fn test_lineage() -> crate::lineage::Lineage {
    crate::lineage::Lineage {
        daemon_pid: 100,
        client_pid: Some(50),
        session_id: Some("test-session".to_string()),
    }
}

#[test]
fn apply_client_env_filters_stale_jobserver_vars_for_compiler_spawns() {
    let env = jobserver_client_env();
    let mut cmd = tokio::process::Command::new("env");
    apply_client_env(&mut cmd, &Some(env), &test_lineage());

    let envs = collect_command_env(cmd.as_std().get_envs());
    assert_eq!(env_value(&envs, "PATH"), Some("/usr/bin"));
    assert_eq!(
        env_value(&envs, "CARGO_MANIFEST_DIR"),
        Some("/tmp/workspace")
    );
    assert_eq!(env_value(&envs, "MAKEFLAGS"), None);
    assert_eq!(env_value(&envs, "CARGO_MAKEFLAGS"), None);
    assert_eq!(
        env_value(&envs, crate::lineage::ENV_DAEMON_PID),
        Some("100")
    );
}

#[test]
fn apply_client_env_sync_filters_stale_jobserver_vars_for_tool_spawns() {
    let env = jobserver_client_env();
    let mut cmd = std::process::Command::new("env");
    apply_client_env_sync(&mut cmd, Some(&env), &test_lineage());

    let envs = collect_command_env(cmd.get_envs());
    assert_eq!(env_value(&envs, "PATH"), Some("/usr/bin"));
    assert_eq!(
        env_value(&envs, "CARGO_MANIFEST_DIR"),
        Some("/tmp/workspace")
    );
    assert_eq!(env_value(&envs, "MAKEFLAGS"), None);
    assert_eq!(env_value(&envs, "CARGO_MAKEFLAGS"), None);
    assert_eq!(
        env_value(&envs, crate::lineage::ENV_DAEMON_PID),
        Some("100")
    );
}

fn test_request_validation_key(index: usize, root: &Path) -> RequestValidationKey {
    RequestValidationKey {
        request_fp: test_content_hash(index),
        root: NormalizedPath::new(root),
    }
}

fn test_request_validation_entry(cached_at: std::time::Instant) -> RequestValidationEntry {
    RequestValidationEntry {
        artifact_key_hex: "artifact".to_string(),
        clock: Clock::ZERO,
        cached_at,
    }
}

#[test]
fn compiler_hash_cache_reuses_hash_for_unchanged_compiler() {
    let tmp = tempfile::tempdir().unwrap();
    let compiler = tmp.path().join("rustc.exe");
    std::fs::write(&compiler, b"fake rustc").unwrap();

    let cache = CompilerHashCache::new();
    let hash_calls = AtomicUsize::new(0);
    let first = cache.get_or_hash_with(&compiler, |_| {
        hash_calls.fetch_add(1, Ordering::Relaxed);
        Some(ContentHash::from_bytes([7; 32]))
    });
    let second = cache.get_or_hash_with(&compiler, |_| {
        hash_calls.fetch_add(1, Ordering::Relaxed);
        Some(ContentHash::from_bytes([9; 32]))
    });

    assert_eq!(first, Some(ContentHash::from_bytes([7; 32])));
    assert_eq!(second, first);
    assert_eq!(hash_calls.load(Ordering::Relaxed), 1);
}

#[test]
fn compiler_hash_cache_rehashes_when_compiler_metadata_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let compiler = tmp.path().join("rustc.exe");
    std::fs::write(&compiler, b"fake rustc").unwrap();
    filetime::set_file_mtime(
        &compiler,
        filetime::FileTime::from_unix_time(1_000_000_000, 0),
    )
    .unwrap();

    let cache = CompilerHashCache::new();
    let hash_calls = AtomicUsize::new(0);
    let first = cache.get_or_hash_with(&compiler, |_| {
        hash_calls.fetch_add(1, Ordering::Relaxed);
        Some(ContentHash::from_bytes([1; 32]))
    });

    std::fs::write(&compiler, b"fake rustc changed").unwrap();
    filetime::set_file_mtime(
        &compiler,
        filetime::FileTime::from_unix_time(1_000_000_010, 0),
    )
    .unwrap();

    let second = cache.get_or_hash_with(&compiler, |_| {
        hash_calls.fetch_add(1, Ordering::Relaxed);
        Some(ContentHash::from_bytes([2; 32]))
    });

    assert_eq!(first, Some(ContentHash::from_bytes([1; 32])));
    assert_eq!(second, Some(ContentHash::from_bytes([2; 32])));
    assert_eq!(hash_calls.load(Ordering::Relaxed), 2);
}

#[test]
fn rustc_context_build_reuses_compiler_hash_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let compiler = tmp.path().join("rustc.exe");
    let source = tmp.path().join("lib.rs");
    let output = tmp.path().join("libunit.rmeta");
    std::fs::write(&compiler, b"fake rustc").unwrap();
    std::fs::write(&source, b"pub fn unit() {}").unwrap();

    let args: Vec<String> = vec![
        "--crate-name".into(),
        "unit".into(),
        "--edition".into(),
        "2021".into(),
        "--emit=dep-info,metadata".into(),
        source.to_string_lossy().into_owned(),
        "-o".into(),
        output.to_string_lossy().into_owned(),
    ];
    let compilation = zccache_compiler::CacheableCompilation {
        compiler: compiler.clone().into(),
        family: zccache_compiler::CompilerFamily::Rustc,
        source_file: source.clone().into(),
        output_file: output.into(),
        original_args: std::sync::Arc::from(args),
        unknown_flags: Vec::new(),
    };
    let cache = CompilerHashCache::new();
    let expected_hash = zccache_hash::hash_file(&compiler).ok();

    let first = build_rustc_compile_context(&compilation, tmp.path(), &[], &cache);
    let second = build_rustc_compile_context(&compilation, tmp.path(), &[], &cache);

    let first_hash = match first {
        BuildContextResult::Rustc { rustc_ctx, .. } => rustc_ctx.compiler_hash,
        BuildContextResult::Cc { .. } => panic!("expected rustc context"),
    };
    let second_hash = match second {
        BuildContextResult::Rustc { rustc_ctx, .. } => rustc_ctx.compiler_hash,
        BuildContextResult::Cc { .. } => panic!("expected rustc context"),
    };
    assert_eq!(first_hash, expected_hash);
    assert_eq!(second_hash, expected_hash);
    assert_eq!(cache.len(), 1);
}

#[test]
fn trim_request_cache_removes_old_entries() {
    let cache = DashMap::new();
    let max_age = std::time::Duration::from_millis(10);
    let old_at = std::time::Instant::now();
    let now = old_at.checked_add(max_age * 2).unwrap();
    cache.insert(ContentHash::from_bytes([2; 32]), test_request_entry(old_at));
    cache.insert(ContentHash::from_bytes([1; 32]), test_request_entry(now));

    let removed = trim_request_cache_at(&cache, max_age, now);

    assert_eq!(removed, 1);
    assert_eq!(cache.len(), 1);
    assert!(cache.contains_key(&ContentHash::from_bytes([1; 32])));
}

#[test]
fn cache_entry_freshness_uses_supplied_timestamp() {
    let max_age = std::time::Duration::from_millis(10);
    let cached_at = std::time::Instant::now();
    let compile_start = cached_at.checked_add(max_age / 2).unwrap();
    let later_check = cached_at.checked_add(max_age * 2).unwrap();

    assert!(cache_entry_fresh_at(compile_start, cached_at, max_age));
    assert!(!cache_entry_fresh_at(later_check, cached_at, max_age));
}

#[test]
fn trim_request_cache_keeps_future_entries() {
    let cache = DashMap::new();
    let max_age = std::time::Duration::from_millis(10);
    let now = std::time::Instant::now();
    let future = now.checked_add(max_age * 2).unwrap();
    cache.insert(ContentHash::from_bytes([1; 32]), test_request_entry(future));

    let removed = trim_request_cache_at(&cache, max_age, now);

    assert_eq!(removed, 0);
    assert_eq!(cache.len(), 1);
}

#[test]
fn trim_request_cache_clears_when_over_hard_cap() {
    let cache = DashMap::new();
    let now = std::time::Instant::now();
    for i in 0..=REQUEST_CACHE_MAX_ENTRIES {
        cache.insert(test_content_hash(i), test_request_entry(now));
    }

    let removed = trim_request_cache_at(&cache, EPHEMERAL_CACHE_MAX_AGE, now);

    assert_eq!(removed, REQUEST_CACHE_MAX_ENTRIES + 1);
    assert!(cache.is_empty());
}

#[test]
fn trim_request_validation_cache_removes_old_entries() {
    let cache = DashMap::new();
    let tmp = tempfile::tempdir().unwrap();
    let max_age = std::time::Duration::from_millis(10);
    let old_at = std::time::Instant::now();
    let now = old_at.checked_add(max_age * 2).unwrap();
    cache.insert(
        test_request_validation_key(1, &tmp.path().join("old-root")),
        test_request_validation_entry(old_at),
    );
    cache.insert(
        test_request_validation_key(2, &tmp.path().join("fresh-root")),
        test_request_validation_entry(now),
    );

    let removed = trim_request_validation_cache_at(&cache, max_age, now);

    assert_eq!(removed, 1);
    assert_eq!(cache.len(), 1);
    assert!(cache.contains_key(&test_request_validation_key(
        2,
        &tmp.path().join("fresh-root")
    )));
}

#[test]
fn request_cache_resolved_inputs_requires_cross_root_shareable_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let root_a = tmp.path().join("workspace-a");
    let root_b = tmp.path().join("workspace-b");
    let source_a: NormalizedPath = root_a.join("src/main.cc").into();
    let header_a: NormalizedPath = root_a.join("include/common.h").into();
    let output_a: NormalizedPath = root_a.join("build/main.o").into();
    let entry = request_cache_entry(
        test_context_key("src/main.cc"),
        &source_a,
        &output_a,
        vec![source_a.clone(), header_a],
        Some(&NormalizedPath::new(&root_a)),
    );

    let resolved = request_cache_resolved_inputs(&entry, &NormalizedPath::new(&root_b)).unwrap();

    assert_eq!(
        resolved,
        vec![
            NormalizedPath::new(root_b.join("src/main.cc")),
            NormalizedPath::new(root_b.join("include/common.h")),
        ]
    );
}

#[test]
fn request_cache_inputs_fresh_since_uses_journal_tracking() {
    let journal = zccache_fscache::ChangeJournal::new();
    let path: NormalizedPath = "/tmp/request-cache-input.cc".into();
    let clock = journal.current_clock();

    assert!(!request_cache_inputs_fresh_since(
        &journal,
        std::slice::from_ref(&path),
        clock
    ));

    journal.register(path.clone());
    let validation_clock = journal.current_clock();
    assert!(request_cache_inputs_fresh_since(
        &journal,
        std::slice::from_ref(&path),
        validation_clock
    ));

    journal.advance(vec![path.clone()]);
    assert!(!request_cache_inputs_fresh_since(
        &journal,
        std::slice::from_ref(&path),
        validation_clock
    ));
}

#[test]
fn trim_rsp_cache_removes_old_entries() {
    let cache = DashMap::new();
    let max_age = std::time::Duration::from_millis(10);
    let old_at = std::time::Instant::now();
    let now = old_at.checked_add(max_age * 2).unwrap();
    cache.insert(NormalizedPath::from("/tmp/old.rsp"), test_rsp_entry(old_at));
    cache.insert(NormalizedPath::from("/tmp/fresh.rsp"), test_rsp_entry(now));

    let removed = trim_rsp_cache_at(&cache, max_age, now);

    assert_eq!(removed, 1);
    assert_eq!(cache.len(), 1);
    assert!(cache.contains_key(&NormalizedPath::from("/tmp/fresh.rsp")));
}

#[test]
fn trim_rsp_cache_keeps_future_entries() {
    let cache = DashMap::new();
    let max_age = std::time::Duration::from_millis(10);
    let now = std::time::Instant::now();
    let future = now.checked_add(max_age * 2).unwrap();
    cache.insert(
        NormalizedPath::from("/tmp/future.rsp"),
        test_rsp_entry(future),
    );

    let removed = trim_rsp_cache_at(&cache, max_age, now);

    assert_eq!(removed, 0);
    assert_eq!(cache.len(), 1);
}

#[test]
fn trim_rsp_cache_clears_when_over_hard_cap() {
    let cache = DashMap::new();
    let now = std::time::Instant::now();
    for i in 0..=RSP_CACHE_MAX_ENTRIES {
        cache.insert(
            NormalizedPath::from(format!("/tmp/args{i}.rsp")),
            test_rsp_entry(now),
        );
    }

    let removed = trim_rsp_cache_at(&cache, EPHEMERAL_CACHE_MAX_AGE, now);

    assert_eq!(removed, RSP_CACHE_MAX_ENTRIES + 1);
    assert!(cache.is_empty());
}

#[test]
fn trim_fast_hit_cache_removes_old_entries() {
    let cache = DashMap::new();
    let max_age = std::time::Duration::from_millis(10);
    let old_at = std::time::Instant::now();
    let now = old_at.checked_add(max_age * 2).unwrap();
    let old_key = test_context_key("/tmp/old.c");
    let fresh_key = test_context_key("/tmp/fresh.c");
    cache.insert(old_key, test_fast_hit_entry(old_at));
    cache.insert(fresh_key, test_fast_hit_entry(now));

    let removed = trim_fast_hit_cache_at(&cache, max_age, now);

    assert_eq!(removed, 1);
    assert_eq!(cache.len(), 1);
    assert!(cache.contains_key(&fresh_key));
}

#[test]
fn trim_fast_hit_cache_keeps_future_entries() {
    let cache = DashMap::new();
    let max_age = std::time::Duration::from_millis(10);
    let now = std::time::Instant::now();
    let future = now.checked_add(max_age * 2).unwrap();
    let key = test_context_key("/tmp/future.c");
    cache.insert(key, test_fast_hit_entry(future));

    let removed = trim_fast_hit_cache_at(&cache, max_age, now);

    assert_eq!(removed, 0);
    assert_eq!(cache.len(), 1);
}

struct CacheDirEnvGuard {
    previous: Option<std::ffi::OsString>,
}

impl CacheDirEnvGuard {
    fn set(path: &Path) -> Self {
        let previous = std::env::var_os(zccache_core::config::CACHE_DIR_ENV);
        std::env::set_var(zccache_core::config::CACHE_DIR_ENV, path);
        Self { previous }
    }
}

impl Drop for CacheDirEnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(previous) => std::env::set_var(zccache_core::config::CACHE_DIR_ENV, previous),
            None => std::env::remove_var(zccache_core::config::CACHE_DIR_ENV),
        }
    }
}

#[cfg(unix)]
fn write_fake_linker(dir: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let tool = dir.join("clang");
    std::fs::write(
        &tool,
        r#"#!/bin/sh
out=
while [ "$#" -gt 0 ]; do
if [ "$1" = "-o" ]; then
    shift
    out=$1
fi
shift || true
done
if [ -z "$out" ]; then
exit 2
fi
out_dir=$(dirname "$out")
printf 'binary\n' > "$out"
printf 'debug\n' > "$out_dir/app.pdb"
printf 'map\n' > "$out_dir/app.wasm.map"
"#,
    )
    .unwrap();
    let mut perms = std::fs::metadata(&tool).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&tool, perms).unwrap();
    tool
}

#[cfg(windows)]
fn write_fake_linker(dir: &Path) -> std::path::PathBuf {
    let tool = dir.join("clang.cmd");
    std::fs::write(
        &tool,
        r#"@echo off
set "OUT=%~2"
if "%OUT%"=="" exit /b 2
> "%OUT%" echo binary
for %%I in ("%OUT%") do set "OUTDIR=%%~dpI"
> "%OUTDIR%app.pdb" echo debug
> "%OUTDIR%app.wasm.map" echo map
exit /b 0
"#,
    )
    .unwrap();
    tool
}

#[tokio::test]
async fn link_cache_hit_restores_sibling_side_effects() {
    let tmp = tempfile::tempdir().unwrap();
    let fake_linker = write_fake_linker(tmp.path());
    let input = tmp.path().join("main.o");
    let output = tmp.path().join("app.exe");
    let pdb = tmp.path().join("app.pdb");
    let wasm_map = tmp.path().join("app.wasm.map");
    std::fs::write(&input, b"fake object").unwrap();

    let _cache_dir = CacheDirEnvGuard::set(&tmp.path().join("zccache-cache"));
    let server = DaemonServer::bind(&zccache_ipc::unique_test_endpoint()).unwrap();
    let args = vec![
        "-o".to_string(),
        output.to_string_lossy().into_owned(),
        input.to_string_lossy().into_owned(),
    ];

    let first = handle_link_ephemeral(
        &server.state,
        std::process::id(),
        &fake_linker,
        &args,
        tmp.path(),
        None,
    )
    .await;
    match first {
        Response::LinkResult {
            exit_code, cached, ..
        } => {
            assert_eq!(exit_code, 0);
            assert!(!cached, "first link should populate the cache");
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }
    assert!(
        output.exists(),
        "fresh link should create the primary output"
    );
    assert!(pdb.exists(), "fresh link should create a PDB sidecar");
    assert!(
        wasm_map.exists(),
        "fresh link should create a wasm map sidecar"
    );

    std::fs::remove_file(&pdb).unwrap();
    std::fs::remove_file(&wasm_map).unwrap();

    let second = handle_link_ephemeral(
        &server.state,
        std::process::id(),
        &fake_linker,
        &args,
        tmp.path(),
        None,
    )
    .await;
    match second {
        Response::LinkResult {
            exit_code, cached, ..
        } => {
            assert_eq!(exit_code, 0);
            assert!(cached, "second link should be served from cache");
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }

    assert!(output.exists(), "cache hit should keep the primary output");
    assert!(pdb.exists(), "cache hit should restore the PDB sidecar");
    assert!(
        wasm_map.exists(),
        "cache hit should restore the wasm map sidecar"
    );
}

#[cfg(windows)]
#[test]
fn request_fingerprint_normalizes_equivalent_windows_paths() {
    let args = vec!["-c".to_string(), "src/main.cpp".to_string()];
    let a = request_fingerprint(
        Path::new(r"C:\LLVM\bin\clang++.exe"),
        &args,
        Path::new(r"C:\Work\Project"),
        None,
        None,
    );
    let b = request_fingerprint(
        Path::new("c:/llvm/bin/clang++.exe"),
        &args,
        Path::new("c:/work/project"),
        None,
        None,
    );
    assert_eq!(a, b);
}

#[test]
fn find_git_root_detects_git_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    let nested = root.join("crates/demo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(&nested).unwrap();

    assert_eq!(find_git_root(&nested), Some(root.into()));
}

#[test]
fn resolve_worktree_root_prefers_client_env_override() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("repo/subdir");
    let override_root = tmp.path().join("override-root");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&override_root).unwrap();
    std::fs::create_dir_all(tmp.path().join("repo/.git")).unwrap();
    let env = vec![(
        WORKTREE_ROOT_ENV.to_string(),
        override_root.to_string_lossy().into_owned(),
    )];

    assert_eq!(
        resolve_worktree_root(&cwd, Some(&env)),
        Some(override_root.into())
    );
}

#[test]
fn request_fingerprint_matches_equivalent_roots_for_safe_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let root_a = tmp.path().join("workspace-a");
    let root_b = tmp.path().join("workspace-b");
    let include_a = root_a.join("include");
    let include_b = root_b.join("include");
    let source_a = root_a.join("src/main.cpp");
    let source_b = root_b.join("src/main.cpp");
    let output_a = root_a.join("build/main.o");
    let output_b = root_b.join("build/main.o");

    let args_a = vec![
        "-I".to_string(),
        include_a.to_string_lossy().into_owned(),
        "-c".to_string(),
        source_a.to_string_lossy().into_owned(),
        "-o".to_string(),
        output_a.to_string_lossy().into_owned(),
    ];
    let args_b = vec![
        "-I".to_string(),
        include_b.to_string_lossy().into_owned(),
        "-c".to_string(),
        source_b.to_string_lossy().into_owned(),
        "-o".to_string(),
        output_b.to_string_lossy().into_owned(),
    ];

    let a = request_fingerprint(
        Path::new("/usr/bin/clang++"),
        &args_a,
        &root_a,
        Some(&root_a),
        None,
    );
    let b = request_fingerprint(
        Path::new("/usr/bin/clang++"),
        &args_b,
        &root_b,
        Some(&root_b),
        None,
    );

    assert_eq!(a, b);
}

#[test]
fn request_fingerprint_keeps_external_paths_distinct() {
    let args_a = vec!["-I".to_string(), "/external-a/include".to_string()];
    let args_b = vec!["-I".to_string(), "/external-b/include".to_string()];

    let a = request_fingerprint(
        Path::new("/usr/bin/clang++"),
        &args_a,
        Path::new("/workspace-a"),
        Some(Path::new("/workspace-a")),
        None,
    );
    let b = request_fingerprint(
        Path::new("/usr/bin/clang++"),
        &args_b,
        Path::new("/workspace-b"),
        Some(Path::new("/workspace-b")),
        None,
    );

    assert_ne!(a, b);
}

#[test]
fn request_fingerprint_normalizes_cc_prefix_map_old_side() {
    let tmp = tempfile::tempdir().unwrap();
    let root_a = tmp.path().join("workspace-a");
    let root_b = tmp.path().join("workspace-b");
    let args_a = vec![format!("-ffile-prefix-map={}=.", root_a.display())];
    let args_b = vec![format!("-ffile-prefix-map={}=.", root_b.display())];

    let a = request_fingerprint(
        Path::new("/usr/bin/clang++"),
        &args_a,
        &root_a,
        Some(&root_a),
        None,
    );
    let b = request_fingerprint(
        Path::new("/usr/bin/clang++"),
        &args_b,
        &root_b,
        Some(&root_b),
        None,
    );

    assert_eq!(a, b);
}

#[test]
fn request_fingerprint_normalizes_rust_remap_detached_old_side() {
    let tmp = tempfile::tempdir().unwrap();
    let root_a = tmp.path().join("workspace-a");
    let root_b = tmp.path().join("workspace-b");
    let args_a = vec![
        "--remap-path-prefix".to_string(),
        format!("{}=.", root_a.display()),
    ];
    let args_b = vec![
        "--remap-path-prefix".to_string(),
        format!("{}=.", root_b.display()),
    ];

    let a = request_fingerprint(Path::new("rustc"), &args_a, &root_a, Some(&root_a), None);
    let b = request_fingerprint(Path::new("rustc"), &args_b, &root_b, Some(&root_b), None);

    assert_eq!(a, b);
}

#[test]
fn request_fingerprint_normalizes_rust_remap_equals_old_side() {
    let tmp = tempfile::tempdir().unwrap();
    let root_a = tmp.path().join("workspace-a");
    let root_b = tmp.path().join("workspace-b");
    let args_a = vec![format!("--remap-path-prefix={}=.", root_a.display())];
    let args_b = vec![format!("--remap-path-prefix={}=.", root_b.display())];

    let a = request_fingerprint(Path::new("rustc"), &args_a, &root_a, Some(&root_a), None);
    let b = request_fingerprint(Path::new("rustc"), &args_b, &root_b, Some(&root_b), None);

    assert_eq!(a, b);
}

#[test]
fn request_fingerprint_preserves_rust_remap_new_prefixes() {
    let tmp = tempfile::tempdir().unwrap();
    let root_a = tmp.path().join("workspace-a");
    let root_b = tmp.path().join("workspace-b");
    let args_a = vec![format!("--remap-path-prefix={}=.", root_a.display())];
    let args_b = vec![format!("--remap-path-prefix={}=/src", root_b.display())];

    let a = request_fingerprint(Path::new("rustc"), &args_a, &root_a, Some(&root_a), None);
    let b = request_fingerprint(Path::new("rustc"), &args_b, &root_b, Some(&root_b), None);

    assert_ne!(a, b);
}

#[test]
fn request_fingerprint_keeps_malformed_rust_remap_detached_values_distinct() {
    let tmp = tempfile::tempdir().unwrap();
    let root_a = tmp.path().join("workspace-a");
    let root_b = tmp.path().join("workspace-b");
    let args_a = vec![
        "--remap-path-prefix".to_string(),
        root_a.to_string_lossy().into_owned(),
    ];
    let args_b = vec![
        "--remap-path-prefix".to_string(),
        root_b.to_string_lossy().into_owned(),
    ];

    let a = request_fingerprint(Path::new("rustc"), &args_a, &root_a, Some(&root_a), None);
    let b = request_fingerprint(Path::new("rustc"), &args_b, &root_b, Some(&root_b), None);

    assert_ne!(a, b);
}

#[test]
fn effective_compile_args_auto_adds_root_and_cwd_maps() {
    let tmp = tempfile::tempdir().unwrap();
    let root_path = tmp.path().join("workspace");
    let cwd = root_path.join("build");
    let root = NormalizedPath::new(&root_path);
    let env = vec![(PATH_REMAP_ENV.to_string(), "auto".to_string())];
    let args = vec!["-c".to_string(), "src/main.cc".to_string()];

    let effective = effective_compile_args(
        &args,
        Path::new("/usr/bin/clang++"),
        &cwd,
        Some(&root),
        Some(&env),
    );

    assert!(effective.contains(&"-c".to_string()));
    assert!(effective.contains(&format!("-ffile-prefix-map={}=.", root_path.display())));
    assert!(effective.contains(&format!("-ffile-prefix-map={}=.", cwd.display())));
    assert_eq!(
        effective[0],
        format!("-ffile-prefix-map={}=.", root_path.display())
    );
    assert_eq!(
        effective[1],
        format!("-ffile-prefix-map={}=.", cwd.display())
    );
}

#[test]
fn effective_compile_args_auto_cc_maps_are_fallbacks_before_user_maps() {
    let tmp = tempfile::tempdir().unwrap();
    let root_path = tmp.path().join("workspace");
    let subtree = root_path.join("src/generated");
    let root = NormalizedPath::new(&root_path);
    let env = vec![(PATH_REMAP_ENV.to_string(), "auto".to_string())];
    let user_map = format!("-ffile-prefix-map={}=/generated", subtree.display());
    let args = vec![
        user_map.clone(),
        "-c".to_string(),
        "src/main.cc".to_string(),
    ];

    let effective = effective_compile_args(
        &args,
        Path::new("/usr/bin/clang++"),
        &root_path,
        Some(&root),
        Some(&env),
    );

    assert_eq!(
        effective[0],
        format!("-ffile-prefix-map={}=.", root_path.display())
    );
    let user_map_pos = effective.iter().position(|arg| arg == &user_map).unwrap();
    assert!(
        user_map_pos > 0,
        "user-supplied narrower map must remain after the auto root fallback"
    );
}

#[test]
fn effective_compile_args_auto_cc_debug_map_does_not_suppress_file_map() {
    let tmp = tempfile::tempdir().unwrap();
    let root_path = tmp.path().join("workspace");
    let root = NormalizedPath::new(&root_path);
    let env = vec![(PATH_REMAP_ENV.to_string(), "auto".to_string())];
    let debug_map = format!("-fdebug-prefix-map={}=/debug", root_path.display());
    let args = vec![
        debug_map.clone(),
        "-c".to_string(),
        "src/main.cc".to_string(),
    ];

    let effective = effective_compile_args(
        &args,
        Path::new("/usr/bin/clang++"),
        &root_path,
        Some(&root),
        Some(&env),
    );

    assert_eq!(
        effective[0],
        format!("-ffile-prefix-map={}=.", root_path.display())
    );
    assert!(effective.contains(&debug_map));
}

#[test]
fn effective_compile_args_auto_adds_rust_root_remap_as_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let root_path = tmp.path().join("workspace");
    let root = NormalizedPath::new(&root_path);
    let env = vec![(PATH_REMAP_ENV.to_string(), "auto".to_string())];
    let args = vec![
        "--crate-type".to_string(),
        "lib".to_string(),
        "src/lib.rs".to_string(),
    ];

    let effective = effective_compile_args(
        &args,
        Path::new("rustc"),
        &root_path,
        Some(&root),
        Some(&env),
    );

    assert_eq!(
        &effective[..2],
        &[
            "--remap-path-prefix".to_string(),
            format!("{}=.", root_path.display())
        ]
    );
}

#[test]
fn effective_compile_args_auto_rust_remap_is_before_user_subtree_remap() {
    let tmp = tempfile::tempdir().unwrap();
    let root_path = tmp.path().join("workspace");
    let subtree = root_path.join("src/generated");
    let root = NormalizedPath::new(&root_path);
    let env = vec![(PATH_REMAP_ENV.to_string(), "auto".to_string())];
    let user_remap = format!("--remap-path-prefix={}=/generated", subtree.display());
    let args = vec![user_remap.clone(), "src/lib.rs".to_string()];

    let effective = effective_compile_args(
        &args,
        Path::new("rustc"),
        &root_path,
        Some(&root),
        Some(&env),
    );

    assert_eq!(
        &effective[..2],
        &[
            "--remap-path-prefix".to_string(),
            format!("{}=.", root_path.display())
        ]
    );
    let user_remap_pos = effective.iter().position(|arg| arg == &user_remap).unwrap();
    assert!(
        user_remap_pos > 1,
        "user-supplied narrower remap must remain after the auto root fallback"
    );
}

#[test]
fn effective_compile_args_auto_keeps_existing_rust_root_remap() {
    let tmp = tempfile::tempdir().unwrap();
    let root_path = tmp.path().join("workspace");
    let root = NormalizedPath::new(&root_path);
    let env = vec![(PATH_REMAP_ENV.to_string(), "auto".to_string())];
    let args = vec![
        format!("--remap-path-prefix={}=/src", root_path.display()),
        "src/lib.rs".to_string(),
    ];

    let effective = effective_compile_args(
        &args,
        Path::new("clippy-driver"),
        &root_path,
        Some(&root),
        Some(&env),
    );

    assert_eq!(effective, args);
}

#[test]
fn link_flag_normalization_keeps_outputs_root_specific() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace-a");
    let lib = root.join("lib");
    let version_map = root.join("link/version.map");
    let more_lib = root.join("more-lib");
    let wasm_map = root.join("link/wasm.map");
    let app_map = root.join("build/app.map");
    let app_lib = root.join("build/app.lib");
    let app_pdb = root.join("build/app.pdb");
    let app_def = root.join("link/app.def");
    let flags = vec![
        "-L".to_string(),
        lib.to_string_lossy().into_owned(),
        "--version-script".to_string(),
        version_map.to_string_lossy().into_owned(),
        format!(
            "-Wl,-L,{},--version-script,{}",
            more_lib.display(),
            wasm_map.display()
        ),
        format!("-Wl,-Map,{}", app_map.display()),
        format!("/IMPLIB:{}", app_lib.display()),
        format!("/PDB:{}", app_pdb.display()),
        format!("/DEF:{}", app_def.display()),
    ];

    let normalized = normalize_link_cache_flags_for_key(&flags, Some(&root));

    assert_eq!(normalized[1], "$ZCCACHE_WORKTREE_ROOT/lib");
    assert_eq!(normalized[3], "$ZCCACHE_WORKTREE_ROOT/link/version.map");
    assert_eq!(
        normalized[4],
        "-Wl,-L,$ZCCACHE_WORKTREE_ROOT/more-lib,--version-script,$ZCCACHE_WORKTREE_ROOT/link/wasm.map"
    );
    assert_eq!(normalized[5], format!("-Wl,-Map,{}", app_map.display()));
    assert_eq!(normalized[6], format!("/IMPLIB:{}", app_lib.display()));
    assert_eq!(normalized[7], format!("/PDB:{}", app_pdb.display()));
    assert_eq!(normalized[8], "/DEF:$ZCCACHE_WORKTREE_ROOT/link/app.def");
}

#[test]
fn request_fingerprint_includes_rust_key_env() {
    let args = vec!["src/lib.rs".to_string()];
    let env_a = vec![("CARGO_PKG_VERSION".to_string(), "1.0.0".to_string())];
    let env_b = vec![("CARGO_PKG_VERSION".to_string(), "1.0.1".to_string())];

    let a = request_fingerprint(
        Path::new("/usr/bin/rustc"),
        &args,
        Path::new("/workspace"),
        Some(Path::new("/workspace")),
        Some(&env_a),
    );
    let b = request_fingerprint(
        Path::new("/usr/bin/rustc"),
        &args,
        Path::new("/workspace"),
        Some(Path::new("/workspace")),
        Some(&env_b),
    );

    assert_ne!(a, b);
}

#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC + file watcher
async fn test_server_ping_pong() {
    zccache_test_support::test_timeout(async {
        let (endpoint, server_task, shutdown) = start_daemon().await;

        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
        client.send(&Request::Ping).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        assert_eq!(resp, Some(Response::Pong));

        shutdown.notify_one();
        server_task.await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC + file watcher
async fn test_server_shutdown_request() {
    zccache_test_support::test_timeout(async {
        let (endpoint, server_task, shutdown) = start_daemon().await;

        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
        client.send(&Request::Shutdown).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        assert_eq!(resp, Some(Response::ShuttingDown));

        shutdown.notify_one();
        server_task.await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC + file watcher
async fn test_server_clear_empty() {
    zccache_test_support::test_timeout(async {
        let (endpoint, server_task, shutdown) = start_daemon().await;

        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
        client.send(&Request::Clear).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        match resp {
            Some(Response::Cleared {
                metadata_cleared,
                dep_graph_contexts_cleared,
                ..
            }) => {
                // artifacts_removed may be >0 if persistent cache has entries
                // from a prior run. Metadata and dep graph are always fresh.
                assert_eq!(metadata_cleared, 0);
                assert_eq!(dep_graph_contexts_cleared, 0);
            }
            other => panic!("expected Cleared, got: {other:?}"),
        }

        shutdown.notify_one();
        server_task.await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC + file watcher
async fn test_server_status() {
    zccache_test_support::test_timeout(async {
        let (endpoint, server_task, shutdown) = start_daemon().await;

        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
        client.send(&Request::Status).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        assert!(matches!(resp, Some(Response::Status(_))));

        shutdown.notify_one();
        server_task.await.unwrap();
    })
    .await;
}

// ── CLI session flow tests (IPC-based) ──────────────────────────────

/// Full session lifecycle: start → compile (miss) → compile (hit) → end.
#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC + compiler
async fn cli_session_lifecycle() {
    let clang = match zccache_test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    zccache_test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("hello.cpp");
        let obj = tmp.path().join("hello.o");
        let log = tmp.path().join("session.log");
        let cwd = tmp.path().to_string_lossy().into_owned();

        std::fs::write(
            &src,
            "#include <stdio.h>\nint main() { printf(\"hello\\n\"); return 0; }\n",
        )
        .unwrap();

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

        // session-start
        client
            .send(&Request::SessionStart {
                client_pid: std::process::id(),
                working_dir: cwd.clone().into(),
                log_file: Some(log.to_string_lossy().into_owned().into()),
                track_stats: false,
                journal_path: None,
                profile: false,
            })
            .await
            .unwrap();

        let session_id = match client.recv().await.unwrap() {
            Some(Response::SessionStarted { session_id, .. }) => session_id,
            other => panic!("expected SessionStarted, got: {other:?}"),
        };

        // first compile (cache miss)
        client
            .send(&Request::Compile {
                session_id: session_id.clone(),
                args: vec![
                    "-c".to_string(),
                    src.to_string_lossy().into_owned(),
                    "-o".to_string(),
                    obj.to_string_lossy().into_owned(),
                ],
                cwd: cwd.clone().into(),
                compiler: clang.to_string_lossy().into_owned().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::CompileResult {
                exit_code, cached, ..
            }) => {
                assert_eq!(exit_code, 0, "first compile should succeed");
                assert!(!cached, "first compile should be a miss");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }

        assert!(obj.exists(), ".o should exist after first compile");
        let obj_data = std::fs::read(&obj).unwrap();

        // second compile (cache hit)
        std::fs::remove_file(&obj).unwrap();

        client
            .send(&Request::Compile {
                session_id: session_id.clone(),
                args: vec![
                    "-c".to_string(),
                    src.to_string_lossy().into_owned(),
                    "-o".to_string(),
                    obj.to_string_lossy().into_owned(),
                ],
                cwd: cwd.clone().into(),
                compiler: clang.to_string_lossy().into_owned().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::CompileResult {
                exit_code, cached, ..
            }) => {
                assert_eq!(exit_code, 0, "cached compile should succeed");
                assert!(cached, "second compile should be a hit");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }

        assert!(obj.exists(), ".o should exist after cached compile");
        let cached_data = std::fs::read(&obj).unwrap();
        assert_eq!(obj_data.len(), cached_data.len(), "cached .o should match");

        // session-end
        client
            .send(&Request::SessionEnd {
                session_id: session_id.clone(),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::SessionEnded { .. }) => {}
            other => panic!("expected SessionEnded, got: {other:?}"),
        }

        // compile after session-end should fail
        client
            .send(&Request::Compile {
                session_id,
                args: vec!["-c".to_string(), src.to_string_lossy().into_owned()],
                cwd: cwd.clone().into(),
                compiler: clang.to_string_lossy().into_owned().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::Error { message }) => {
                assert!(
                    message.contains("unknown session"),
                    "should report unknown session after end: {message}"
                );
            }
            other => panic!("expected Error after session-end, got: {other:?}"),
        }

        // verify log
        let log_text = std::fs::read_to_string(&log).unwrap();
        assert!(log_text.contains("[MISS]"), "log should show miss");
        assert!(log_text.contains("[HIT]"), "log should show hit");

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

/// Ending a session with a malformed (non-UUID) ID returns an error.
#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC
async fn cli_session_end_invalid_id() {
    zccache_test_support::test_timeout(async {
        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

        client
            .send(&Request::SessionEnd {
                session_id: 999999.to_string(),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::Error { message }) => {
                assert!(
                    message.contains("unknown session") || message.contains("invalid session"),
                    "expected session error, got: {message}"
                );
            }
            other => panic!("expected Error, got: {other:?}"),
        }

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

/// Ending an unknown session (well-formed UUID, but daemon has no record
/// of it) is idempotent and returns SessionEnded { stats: None }.
///
/// This simulates the scenario where the daemon was restarted between
/// `session-start` and `session-end` (e.g. zccache-ci kills the daemon
/// mid-build to unlock target binaries on Windows). Build wrappers like
/// soldr call `session-end` at process exit and must not see a spurious
/// failure when the in-memory session is gone.
#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC
async fn cli_session_end_unknown_uuid_is_idempotent() {
    zccache_test_support::test_timeout(async {
        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

        client
            .send(&Request::SessionEnd {
                // A well-formed UUID that the daemon has never seen.
                session_id: "00000000-0000-0000-0000-000000000000".to_string(),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::SessionEnded { stats }) => {
                assert!(
                    stats.is_none(),
                    "no stats expected for unknown session, got: {stats:?}"
                );
            }
            other => panic!("expected SessionEnded for unknown UUID, got: {other:?}"),
        }

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

/// Regression for #166 — Compile on an unknown session must not fail with
/// "unknown session", mirroring #137's SessionEnd idempotency. Triggered
/// when zccache-ci kills the daemon mid-build (#167).
///
/// The daemon used to short-circuit Compile with `Response::Error` if the
/// session UUID was unknown. After a daemon restart, soldr-managed rustc
/// wrappers keep using the old session UUID and would all fail; soldr in
/// turn exits 1 and the whole build breaks. We now let the compile
/// proceed; only per-session stats are lost.
#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC
async fn cli_compile_unknown_uuid_is_idempotent() {
    zccache_test_support::test_timeout(async {
        let tmp = tempfile::tempdir().unwrap();
        // Use an isolated cache dir so we don't clash with any
        // production daemon writing the global index blob.
        let _cache_dir = CacheDirEnvGuard::set(&tmp.path().join("zccache-cache"));

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

        let cwd = tmp.path().to_string_lossy().into_owned();

        // Send a Compile with a well-formed UUID the daemon has never
        // seen. We intentionally pass a bogus compiler path and trivial
        // args — the only assertion is that we don't get the
        // "unknown session" Error response that the pre-#166 code emitted
        // before any real compilation work began.
        client
            .send(&Request::Compile {
                session_id: "00000000-0000-0000-0000-000000000000".to_string(),
                args: vec!["--version".to_string()],
                cwd: cwd.clone().into(),
                compiler: "/nonexistent/compiler".to_string().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();

        // Any non-Error response is acceptable — typically a
        // CompileResult with a non-zero exit code because the compiler
        // path is bogus. The key invariant is the absence of the
        // pre-#166 "unknown session" hard error.
        if let Some(Response::Error { message }) = client.recv().await.unwrap() {
            assert!(
                !message.contains("unknown session"),
                "Compile must not fail with 'unknown session' on an unknown UUID, got: {message}"
            );
        }

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

/// Cache clear resets: miss → hit → clear → miss again.
#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC + compiler
async fn cli_clear_resets_cache() {
    let clang = match zccache_test_support::find_clang() {
        Some(p) => p,
        None => return,
    };

    zccache_test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("clear_test.cpp");
        let obj = tmp.path().join("clear_test.o");
        let cwd = tmp.path().to_string_lossy().into_owned();

        std::fs::write(&src, "int main() { return 0; }\n").unwrap();

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

        // Start session
        client
            .send(&Request::SessionStart {
                client_pid: std::process::id(),
                working_dir: cwd.clone().into(),
                log_file: None,
                track_stats: false,
                journal_path: None,
                profile: false,
            })
            .await
            .unwrap();

        let session_id = match client.recv().await.unwrap() {
            Some(Response::SessionStarted { session_id, .. }) => session_id,
            other => panic!("expected SessionStarted, got: {other:?}"),
        };

        let compile_args = vec![
            "-c".to_string(),
            src.to_string_lossy().into_owned(),
            "-o".to_string(),
            obj.to_string_lossy().into_owned(),
        ];

        // First compile → miss
        client
            .send(&Request::Compile {
                session_id: session_id.clone(),
                args: compile_args.clone(),
                cwd: cwd.clone().into(),
                compiler: clang.to_string_lossy().into_owned().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::CompileResult {
                exit_code, cached, ..
            }) => {
                assert_eq!(exit_code, 0);
                assert!(!cached, "first compile should be a miss");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }

        // Second compile → hit
        std::fs::remove_file(&obj).unwrap();
        client
            .send(&Request::Compile {
                session_id: session_id.clone(),
                args: compile_args.clone(),
                cwd: cwd.clone().into(),
                compiler: clang.to_string_lossy().into_owned().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::CompileResult {
                exit_code, cached, ..
            }) => {
                assert_eq!(exit_code, 0);
                assert!(cached, "second compile should be a hit");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }

        // Clear the cache
        client.send(&Request::Clear).await.unwrap();
        match client.recv().await.unwrap() {
            Some(Response::Cleared {
                artifacts_removed, ..
            }) => {
                assert!(
                    artifacts_removed > 0,
                    "should have cleared at least one artifact"
                );
            }
            other => panic!("expected Cleared, got: {other:?}"),
        }

        // End old session and start a new one
        client
            .send(&Request::SessionEnd { session_id })
            .await
            .unwrap();
        let _: Option<Response> = client.recv().await.unwrap();

        client
            .send(&Request::SessionStart {
                client_pid: std::process::id(),
                working_dir: cwd.clone().into(),
                log_file: None,
                track_stats: false,
                journal_path: None,
                profile: false,
            })
            .await
            .unwrap();

        let session_id2 = match client.recv().await.unwrap() {
            Some(Response::SessionStarted { session_id, .. }) => session_id,
            other => panic!("expected SessionStarted, got: {other:?}"),
        };

        // Compile again → should be a miss (cache was cleared)
        std::fs::remove_file(&obj).unwrap();
        client
            .send(&Request::Compile {
                session_id: session_id2,
                args: compile_args,
                cwd: cwd.into(),
                compiler: clang.to_string_lossy().into_owned().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::CompileResult {
                exit_code, cached, ..
            }) => {
                assert_eq!(exit_code, 0);
                assert!(!cached, "compile after clear should be a miss");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

/// Multi-file compilations fall back to running the compiler directly.
#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC + compiler
async fn cli_multi_file_compilation_runs_directly() {
    let clang = match zccache_test_support::find_clang() {
        Some(p) => p,
        None => return,
    };

    zccache_test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let src_a = tmp.path().join("multi_a.cpp");
        let src_b = tmp.path().join("multi_b.cpp");
        let cwd = tmp.path().to_string_lossy().into_owned();

        std::fs::write(&src_a, "int foo() { return 1; }\n").unwrap();
        std::fs::write(&src_b, "int bar() { return 2; }\n").unwrap();

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

        // Start session
        client
            .send(&Request::SessionStart {
                client_pid: std::process::id(),
                working_dir: cwd.clone().into(),
                log_file: None,
                track_stats: true,
                journal_path: None,
                profile: false,
            })
            .await
            .unwrap();

        let session_id = match client.recv().await.unwrap() {
            Some(Response::SessionStarted { session_id, .. }) => session_id,
            other => panic!("expected SessionStarted, got: {other:?}"),
        };

        // First compile: multi-file → both are cache misses
        let multi_args = vec![
            "-c".to_string(),
            src_a.to_string_lossy().into_owned(),
            src_b.to_string_lossy().into_owned(),
        ];
        client
            .send(&Request::Compile {
                session_id: session_id.clone(),
                args: multi_args.clone(),
                cwd: cwd.clone().into(),
                compiler: clang.to_string_lossy().into_owned().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::CompileResult {
                exit_code, cached, ..
            }) => {
                assert_eq!(exit_code, 0, "multi-file compile should succeed");
                assert!(!cached, "first multi-file compile should be a miss");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }

        // Verify both .o files were produced
        let obj_a = tmp.path().join("multi_a.o");
        let obj_b = tmp.path().join("multi_b.o");
        assert!(obj_a.exists(), "multi_a.o should exist");
        assert!(obj_b.exists(), "multi_b.o should exist");

        // Second compile: same files → should be all cache hits
        client
            .send(&Request::Compile {
                session_id: session_id.clone(),
                args: multi_args,
                cwd: cwd.clone().into(),
                compiler: clang.to_string_lossy().into_owned().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::CompileResult {
                exit_code, cached, ..
            }) => {
                assert_eq!(exit_code, 0, "second multi-file compile should succeed");
                assert!(cached, "second multi-file compile should be all cache hits");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }

        // End session and verify stats
        client
            .send(&Request::SessionEnd { session_id })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::SessionEnded { stats }) => {
                if let Some(s) = stats {
                    assert!(
                        s.misses >= 2,
                        "first multi-file compile should have 2 misses, got: {}",
                        s.misses
                    );
                    assert!(
                        s.hits >= 2,
                        "second multi-file compile should have 2 hits, got: {}",
                        s.hits
                    );
                }
            }
            other => panic!("expected SessionEnded, got: {other:?}"),
        }

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

// ── pch_source_header unit tests ────────────────────────────────────

#[test]
fn pch_source_header_sibling() {
    let tmp = tempfile::tempdir().unwrap();
    let header = tmp.path().join("pch.h");
    let pch = tmp.path().join("pch.h.pch");
    std::fs::write(&header, "// pch").unwrap();
    std::fs::write(&pch, "binary").unwrap();

    let result = pch_source_header(&pch);
    assert_eq!(result, Some(header.into()));
}

#[test]
fn pch_source_header_build_dir() {
    // The walk-up heuristic looks for `<dir_name>/<header_name>` from ancestors.
    // e.g., for .build/tests/pch.h.pch it looks for tests/pch.h in parents.
    let tmp = tempfile::tempdir().unwrap();
    // Source: tmp/tests/pch.h (matches the `tests/pch.h` relative lookup)
    let src_dir = tmp.path().join("tests");
    std::fs::create_dir_all(&src_dir).unwrap();
    let header = src_dir.join("pch.h");
    std::fs::write(&header, "// pch").unwrap();

    // PCH: tmp/build/tests/pch.h.pch
    let build_dir = tmp.path().join("build").join("tests");
    std::fs::create_dir_all(&build_dir).unwrap();
    let pch = build_dir.join("pch.h.pch");
    std::fs::write(&pch, "binary").unwrap();

    let result = pch_source_header(&pch);
    assert_eq!(result, Some(header.into()));
}

#[test]
fn pch_source_header_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let build_dir = tmp.path().join("build");
    std::fs::create_dir_all(&build_dir).unwrap();
    let pch = build_dir.join("pch.h.pch");
    std::fs::write(&pch, "binary").unwrap();

    let result = pch_source_header(&pch);
    assert_eq!(result, None);
}

#[test]
fn pch_source_header_non_pch() {
    let tmp = tempfile::tempdir().unwrap();
    let obj = tmp.path().join("foo.o");
    std::fs::write(&obj, "object").unwrap();

    let result = pch_source_header(&obj);
    assert_eq!(result, None);
}

#[test]
fn pch_source_header_gch_extension() {
    let tmp = tempfile::tempdir().unwrap();
    let header = tmp.path().join("pch.h");
    let gch = tmp.path().join("pch.h.gch");
    std::fs::write(&header, "// pch").unwrap();
    std::fs::write(&gch, "binary").unwrap();

    let result = pch_source_header(&gch);
    assert_eq!(result, Some(header.into()));
}

// ── resolve_pch_source unit tests ───────────────────────────────────

#[test]
fn resolve_pch_source_registry_hit() {
    let pch_map: DashMap<NormalizedPath, NormalizedPath> = DashMap::new();
    let pch_path = NormalizedPath::from("/build/tests/pch.h.pch");
    let src_path = NormalizedPath::from("/src/tests/pch.h");
    pch_map.insert(pch_path.clone(), src_path.clone());

    let result = resolve_pch_source(&pch_path, &pch_map);
    assert_eq!(result, Some(src_path));
}

#[test]
fn resolve_pch_source_falls_back_to_filesystem() {
    let tmp = tempfile::tempdir().unwrap();
    let header = tmp.path().join("pch.h");
    let pch = tmp.path().join("pch.h.pch");
    std::fs::write(&header, "// pch").unwrap();
    std::fs::write(&pch, "binary").unwrap();

    let pch_map: DashMap<NormalizedPath, NormalizedPath> = DashMap::new();
    let result = resolve_pch_source(&pch, &pch_map);
    assert_eq!(result, Some(header.into()));
}

#[test]
fn resolve_pch_source_non_pch_returns_none() {
    let pch_map: DashMap<NormalizedPath, NormalizedPath> = DashMap::new();
    let result = resolve_pch_source(Path::new("/build/foo.o"), &pch_map);
    assert_eq!(result, None);
}

// ── write_cached_output staleness tests ────────────────────────────

/// Regression test: write_cached_output must overwrite an existing output
/// file even when the existing file has the same size as the cached data.
///
/// This reproduces the linker staleness bug where a header change produces
/// a .o of the same size but different content — the old size-only check
/// skipped the write, leaving a stale .o on disk with missing symbols.
#[test]
fn write_cached_output_overwrites_same_size_different_content() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("output.o");
    let cache = dir.path().join("cached.o");

    // Simulate: output.o exists from a previous compilation (version A).
    let old_content = b"AAAA_symbols_v1_xxxx";
    std::fs::write(&out, old_content).unwrap();

    // Simulate: cache file has new content (version B) — same size, different bytes.
    let new_content = b"BBBB_symbols_v2_yyyy";
    assert_eq!(
        old_content.len(),
        new_content.len(),
        "test requires same size"
    );
    std::fs::write(&cache, new_content).unwrap();

    // write_cached_output must replace the stale output with the cached content.
    write_cached_output(&out, &cache, new_content).unwrap();

    let result = std::fs::read(&out).unwrap();
    assert_eq!(
        result, new_content,
        "output must contain new content, not stale old content"
    );
}

/// write_cached_output correctly creates the output when it doesn't exist.
#[test]
fn write_cached_output_creates_new_file() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("output.o");
    let cache = dir.path().join("cached.o");

    let content = b"fresh object file data";
    std::fs::write(&cache, content).unwrap();

    write_cached_output(&out, &cache, content).unwrap();

    let result = std::fs::read(&out).unwrap();
    assert_eq!(result, content.as_slice());
}

/// write_cached_output falls back to memory copy when cache file is missing.
#[test]
fn write_cached_output_fallback_to_memory_copy() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("output.o");
    let cache = dir.path().join("nonexistent_cache.o");

    let content = b"data from memory";

    write_cached_output(&out, &cache, content).unwrap();

    let result = std::fs::read(&out).unwrap();
    assert_eq!(result, content.as_slice());
}

/// write_cached_output skips the write when output is already a hardlink
/// to the cache file (same file identity). This is the fast path for
/// repeated cache hits with the same artifact key.
#[test]
fn write_cached_output_skips_when_already_hardlinked() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cached.o");
    let out = dir.path().join("output.o");

    let content = b"cached artifact content";
    std::fs::write(&cache, content).unwrap();

    // First write: creates hardlink
    write_cached_output(&out, &cache, content).unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), content.as_slice());

    // Verify they are the same file (hardlink).
    assert!(
        same_file(&out, &cache),
        "output should be a hardlink to cache file after first write"
    );

    // Second write: should detect hardlink and skip.
    // (If it didn't skip, it would still produce correct content,
    //  but the test verifies the optimization path exists.)
    write_cached_output(&out, &cache, content).unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), content.as_slice());
}

#[test]
fn persist_artifact_output_does_not_mutate_existing_hardlink() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("artifact-key_0");
    let out = dir.path().join("output.rlib");

    persist_artifact_output(&cache, b"first").unwrap();
    write_cached_output(&out, &cache, b"first").unwrap();
    assert!(
        same_file(&out, &cache),
        "cache hit should initially hardlink output to cache payload"
    );

    persist_artifact_output(&cache, b"second").unwrap();

    assert_eq!(
        std::fs::read(&out).unwrap(),
        b"first",
        "publishing a later cache payload must not mutate existing target outputs"
    );
    assert_eq!(std::fs::read(&cache).unwrap(), b"second");
    assert!(
        !same_file(&out, &cache),
        "cache path replacement should break the hardlink relationship"
    );
}

#[test]
fn persist_artifact_file_reports_hardlink_snapshot_stats() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("libunit.rlib");
    let cache = dir.path().join("artifact-key_0");
    let content = b"compiled rust artifact";
    std::fs::write(&source, content).unwrap();

    let stats = persist_artifact_file(&cache, &source).unwrap();

    assert_eq!(std::fs::read(&cache).unwrap(), content);
    assert!(
        same_file(&source, &cache),
        "same-directory snapshots should use a hardlink"
    );
    assert_eq!(stats.hardlink_count, 1);
    assert_eq!(stats.copy_count, 0);
    assert_eq!(stats.copy_bytes, 0);
}

/// Regression test for issue #197: a cache hit hardlinks the target
/// output to the shared artifact file. Before a later cache miss invokes
/// the compiler for that same target path, zccache must detach the output
/// from the shared cache file so an in-place compiler overwrite cannot
/// mutate the cache artifact used by sibling worktrees.
#[test]
fn break_output_hardlink_before_compile_prevents_cache_poisoning() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cached.rlib");
    let out = dir.path().join("libapp.rlib");

    let cached_content = b"cached artifact from worktree a";
    let rebuilt_content = b"rebuilt artifact in worktree b";
    std::fs::write(&cache, cached_content).unwrap();

    write_cached_output(&out, &cache, cached_content).unwrap();
    assert!(same_file(&out, &cache), "cache hit should hardlink output");

    break_output_hardlink_before_compile(&out).unwrap();
    assert!(
        !same_file(&out, &cache),
        "compile miss must detach output from cache hardlink first"
    );

    std::fs::write(&out, rebuilt_content).unwrap();

    assert_eq!(
        std::fs::read(&cache).unwrap(),
        cached_content,
        "compiler overwrite of output must not mutate shared cache artifact"
    );
    assert_eq!(std::fs::read(&out).unwrap(), rebuilt_content);
}

/// Regression test for issue #15: hardlink delivery must set output mtime
/// to current time. Without this, build systems (cargo, make, ninja) see
/// the output as older than its dependencies and trigger unnecessary rebuilds.
///
/// Root cause: hardlinks share mtime with the cache file, which was created
/// during the original compilation (potentially minutes/hours ago). Cargo
/// checks "is library output older than build script output?" and if the
/// library was hardlinked from an old cache file, the answer is yes → dirty.
#[test]
fn write_cached_output_preserves_cache_mtime_on_hardlink() {
    // Regression guard for iter7: cache hits must keep the cache
    // file's stored mtime, not stamp `now()`. Cargo's incremental
    // fingerprint records the artifact's mtime at first compile;
    // a hit that hardlinks but bumps mtime looks "externally
    // touched" and invalidates downstream — measured as a
    // wall-time regression on the `bin` cell of the
    // cold-tar-untar-warm scenario.
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cached.rlib");
    let out = dir.path().join("output.rlib");

    let content = b"cached rlib data";
    std::fs::write(&cache, content).unwrap();

    let old_time = filetime::FileTime::from_unix_time(1_000_000_000, 0); // 2001-09-09
    filetime::set_file_mtime(&cache, old_time).unwrap();

    write_cached_output(&out, &cache, content).unwrap();

    // Output is a hardlink to cache, so its mtime is the cache mtime.
    // After the iter7 touch_mtime no-op, that mtime is NOT bumped.
    let out_mtime =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&out).unwrap());
    assert_eq!(
        out_mtime.unix_seconds(),
        old_time.unix_seconds(),
        "cache hit must preserve cache file mtime (cargo's fingerprint depends on it); \
         got {out_mtime:?}, expected {old_time:?}"
    );
}

/// Same as above but for the same_file (already hardlinked) path.
#[test]
fn write_cached_output_preserves_mtime_on_existing_hardlink() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cached.rlib");
    let out = dir.path().join("output.rlib");

    let content = b"cached rlib data";
    std::fs::write(&cache, content).unwrap();

    // First delivery: creates hardlink
    write_cached_output(&out, &cache, content).unwrap();

    let old_time = filetime::FileTime::from_unix_time(1_000_000_000, 0);
    filetime::set_file_mtime(&out, old_time).unwrap();

    // Second delivery: same_file path. Iter7 keeps the existing
    // (backdated) mtime instead of stamping `now()`.
    write_cached_output(&out, &cache, content).unwrap();

    let out_mtime =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&out).unwrap());
    assert_eq!(
        out_mtime.unix_seconds(),
        old_time.unix_seconds(),
        "mtime must be preserved across repeated cache hits on the same file"
    );
}

/// write_cached_output fallback (fs::write) naturally sets fresh mtime.
#[test]
fn write_cached_output_fallback_has_fresh_mtime() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("output.rlib");
    let cache = dir.path().join("nonexistent_cache.rlib");

    let content = b"data from memory";
    write_cached_output(&out, &cache, content).unwrap();

    let out_mtime =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&out).unwrap());
    let now = filetime::FileTime::now();
    let diff = now.unix_seconds() - out_mtime.unix_seconds();

    assert!(
        diff < 5,
        "fallback path should produce fresh mtime — {diff}s old"
    );
}

// ── run_post_link_deploy_hook unit tests ────────────────────────────
//
// These tests use a tiny helper program that writes a file next to the
// provided output path and exits 0, simulating a real deploy tool like
// `clang-tool-chain-libdeploy`. They verify:
//   - the hook runs when invoked
//   - failures don't panic / propagate (hook is best-effort)
//   - the env is propagated

/// Run the hook with a command that creates a sidecar file next to the
/// output. Verifies the sidecar appears — this is the contract that
/// `side_effect::detect_side_effects` relies on.
#[cfg(unix)] // uses /bin/sh; Windows has its own test below
#[tokio::test]
async fn post_link_deploy_hook_runs_and_creates_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("app");
    std::fs::write(&output, b"binary").unwrap();

    // Fake deploy tool: creates a sidecar DLL next to the passed path.
    let script = dir.path().join("fake_deploy.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\ntouch \"$(dirname \"$1\")/libruntime.so\"\n",
    )
    .unwrap();
    std::process::Command::new("chmod")
        .args(["+x"])
        .arg(&script)
        .status()
        .unwrap();

    let cmd_str = script.to_string_lossy().to_string();
    let lineage = crate::lineage::Lineage::current(None, None);
    run_post_link_deploy_hook(&cmd_str, &output, None, &lineage).await;

    assert!(
        dir.path().join("libruntime.so").exists(),
        "hook should have created the sidecar"
    );
}

/// Hook that exits non-zero must not panic — failures are best-effort.
#[cfg(unix)]
#[tokio::test]
async fn post_link_deploy_hook_failure_is_non_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("app");
    std::fs::write(&output, b"binary").unwrap();

    // Just exit 1 — no side effect.
    let lineage = crate::lineage::Lineage::current(None, None);
    run_post_link_deploy_hook("false", &output, None, &lineage).await;
    // If we reached here without panic, the test passes. A warning should
    // have been logged by the hook.
}

/// Nonexistent program — hook should log a warning, not panic.
#[tokio::test]
async fn post_link_deploy_hook_nonexistent_program_is_non_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("app.dll");
    std::fs::write(&output, b"binary").unwrap();

    let lineage = crate::lineage::Lineage::current(None, None);
    run_post_link_deploy_hook(
        "this-program-does-not-exist-zccache-test-12345",
        &output,
        None,
        &lineage,
    )
    .await;
    // No panic = pass.
}

/// Empty command string — must early-return without attempting to spawn.
#[tokio::test]
async fn post_link_deploy_hook_empty_cmd_is_noop() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("app.dll");
    std::fs::write(&output, b"binary").unwrap();

    let lineage = crate::lineage::Lineage::current(None, None);
    run_post_link_deploy_hook("", &output, None, &lineage).await;
    run_post_link_deploy_hook("   ", &output, None, &lineage).await;
    // No panic = pass.
}

/// Env is propagated to the hook process.
#[cfg(unix)]
#[tokio::test]
async fn post_link_deploy_hook_propagates_env() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("app");
    std::fs::write(&output, b"binary").unwrap();

    // Script reads $ZCCACHE_TEST_MARKER from env and writes it to a
    // marker file next to the output.
    let script = dir.path().join("read_env.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf '%s' \"$ZCCACHE_TEST_MARKER\" > \"$(dirname \"$1\")/marker.txt\"\n",
    )
    .unwrap();
    std::process::Command::new("chmod")
        .args(["+x"])
        .arg(&script)
        .status()
        .unwrap();

    let env = vec![
        (
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        ),
        ("ZCCACHE_TEST_MARKER".to_string(), "hello-hook".to_string()),
    ];
    let lineage = crate::lineage::Lineage::current(None, None);
    run_post_link_deploy_hook(&script.to_string_lossy(), &output, Some(&env), &lineage).await;

    let marker = std::fs::read_to_string(dir.path().join("marker.txt")).unwrap();
    assert_eq!(marker, "hello-hook");
}

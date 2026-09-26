//! Primary-output destination on a compile cache hit (#1648).
//!
//! The cache key deliberately excludes the `-o` path, so one artifact serves
//! every output name. A C/C++ hit must therefore write the primary payload to
//! the *current* request's `-o` destination. Only rustc may have observed a
//! physical name that differs from its declared primary path (#1522, e.g. an
//! extensionless Wasm plan that emitted `<crate>.wasm`).

use super::*;

struct Fixture {
    _server: crate::daemon::server::DaemonServer,
    state: Arc<SharedState>,
    sid: SessionId,
    dir: tempfile::TempDir,
    payload: Arc<Vec<u8>>,
}

/// Cache one single-output artifact under `key` whose recorded primary name
/// is `cached_name`, as a cold miss for `-o <cached_name>` would store it.
fn fixture(key: &str, cached_name: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let server = crate::daemon::server::tests::bind_isolated_server(dir.path());
    let state = Arc::clone(&server.state);
    let payload = Arc::new(b"cached primary output".to_vec());
    let cache_path = state.artifact_dir.join(format!("{key}_0"));
    let _ = crate::platform::fs::permissions::make_writable(&cache_path);
    std::fs::write(&cache_path, payload.as_slice()).unwrap();
    write_authoritative_blob_digest(&cache_path).unwrap();
    let sid = state.sessions.create(crate::depgraph::SessionConfig {
        client_pid: std::process::id(),
        working_dir: dir.path().into(),
        log_file: None,
        track_stats: true,
        journal_path: None,
        profile: false,
        private_env: Vec::new(),
        owner_pids: Vec::new(),
    });
    let meta = ArtifactIndex::new(
        vec![cached_name.to_string()],
        vec![payload.len() as u64],
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        0,
    );
    state.artifacts.insert(
        key.to_string(),
        CachedArtifact::from_file_payloads(meta, vec![cache_path]),
    );
    Fixture {
        _server: server,
        state,
        sid,
        dir,
        payload,
    }
}

fn materialize(
    fx: &Fixture,
    key: &str,
    output_path: &NormalizedPath,
    rustc_archive_hardlink_eligible: Option<bool>,
) {
    let source_path: NormalizedPath = fx.dir.path().join("source.cc").into();
    let secondary_output_dir: NormalizedPath = output_path
        .parent()
        .unwrap_or(fx.dir.path())
        .to_path_buf()
        .into();
    let response = materialize_cached_compile_hit(CachedHitMaterializeRequest {
        state: &fx.state,
        sid: &fx.sid,
        artifact_key_hex: key,
        verdict_key_hex: None,
        source_path: &source_path,
        output_path,
        secondary_output_dir,
        current_depfile_dest: None,
        compile_start: Instant::now(),
        hit_label: "HIT_TEST",
        cached_error_label: "CACHED_ERROR_TEST",
        record_compilation: true,
        downgrade_output_metadata: false,
        mtime_floor_paths: Vec::new(),
        rustc_metadata_compat_outputs: None,
        rustc_archive_hardlink_eligible,
        materialization_mode: MaterializationMode::Auto,
        phases: CachedHitPhases::request_cache(0, 0),
    })
    .expect("cache hit must materialize");
    assert!(matches!(
        response,
        Response::CompileResult {
            cached: true,
            exit_code: 0,
            ..
        }
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn cc_hit_writes_primary_output_to_the_requested_path_not_the_cached_name() {
    let fx = fixture("cc-rename-key", "a.o");
    let requested: NormalizedPath = fx.dir.path().join("b.o").into();

    materialize(&fx, "cc-rename-key", &requested, None);

    assert_eq!(
        std::fs::read(&requested).unwrap(),
        fx.payload.as_slice(),
        "a C/C++ hit must write the requested -o destination"
    );
    assert!(
        !fx.dir.path().join("a.o").exists(),
        "a C/C++ hit must not write the cold miss's output name"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn cc_hit_with_extensionless_request_keeps_the_exact_requested_name() {
    let fx = fixture("cc-ext-key", "prog.o");
    let requested: NormalizedPath = fx.dir.path().join("prog").into();

    materialize(&fx, "cc-ext-key", &requested, None);

    assert_eq!(std::fs::read(&requested).unwrap(), fx.payload.as_slice());
    assert!(!fx.dir.path().join("prog.o").exists());
}

#[tokio::test(flavor = "current_thread")]
async fn rustc_hit_restores_the_compiler_observed_primary_suffix() {
    let fx = fixture("rustc-wasm-key", "wasm_restore.wasm");
    let requested: NormalizedPath = fx.dir.path().join("wasm_restore").into();

    materialize(&fx, "rustc-wasm-key", &requested, Some(false));

    assert_eq!(
        std::fs::read(fx.dir.path().join("wasm_restore.wasm")).unwrap(),
        fx.payload.as_slice(),
        "#1522: rustc's observed Wasm suffix must survive a hit"
    );
}

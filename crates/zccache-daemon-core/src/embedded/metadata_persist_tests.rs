//! zccache#1652: the embedded service must persist `metadata.bin` so the
//! next start restores the stat-verified hash fast path instead of starting
//! empty every time.
//!
//! Each test drives a real compile through the public facade, ends the
//! service (graceful shutdown, or a drop without shutdown), and then asserts
//! the snapshot exists on disk, decodes, and contains the compiled source.

use super::*;
use std::path::PathBuf;
use tempfile::TempDir;

async fn start_service(cache_root: &std::path::Path) -> ZccacheService {
    let mut audit = AuditConfig::default();
    audit.mode = crate::audit::AuditMode::Off;
    ZccacheService::start(ZccacheConfig {
        host: HostIdentity {
            product: "metadata-persist-test".into(),
            instance_id: uuid::Uuid::new_v4().to_string(),
            workspace_id: "metadata-persist-workspace".into(),
        },
        cache_root: cache_root.into(),
        audit,
        limits: ServiceLimits::default(),
        runtime: RuntimeHooks::default(),
        cancellation: None,
    })
    .await
    .expect("service start")
}

struct Fixture {
    _temp: TempDir,
    cache_root: PathBuf,
    source: PathBuf,
    request: CompileRequest,
}

fn fixture(compiler: crate::core::NormalizedPath) -> Fixture {
    let temp = TempDir::new().expect("tempdir");
    let cache_root = temp.path().join("cache");
    let work = temp.path().join("work");
    std::fs::create_dir_all(&work).expect("work dir");
    let header = work.join("persist.h");
    let source = work.join("persist.cpp");
    let object = work.join("persist.o");
    std::fs::write(&header, "#define PERSIST_VALUE 11\n").expect("header");
    std::fs::write(
        &source,
        "#include \"persist.h\"\nint persist(void) { return PERSIST_VALUE; }\n",
    )
    .expect("source");
    let request = CompileRequest {
        audit: AuditContext::new(
            crate::audit::AuditId::new("metadata-persist-run").expect("id"),
            crate::audit::AuditId::new("metadata-persist-trace").expect("id"),
        ),
        compiler,
        args: vec![
            "-c".into(),
            source.to_string_lossy().into_owned(),
            "-o".into(),
            object.to_string_lossy().into_owned(),
        ],
        cwd: work.into(),
        env: Vec::new(),
        stdin: Vec::new(),
    };
    Fixture {
        _temp: temp,
        cache_root,
        source,
        request,
    }
}

/// Assert `metadata.bin` exists under the cache root, decodes at the current
/// format, and carries a hash for the compiled source.
fn assert_metadata_persisted(cache_root: &std::path::Path, source: &std::path::Path) {
    let path = crate::core::config::metadata_path_from_cache_dir(
        &crate::core::NormalizedPath::new(cache_root),
    );
    assert!(
        path.exists(),
        "metadata.bin must be persisted under the embedded cache root: {}",
        path.display()
    );
    let restored =
        crate::fscache::MetadataCache::load_from_disk(path.as_path()).expect("metadata.bin loads");
    assert!(
        restored
            .get_cached_hash(&crate::core::NormalizedPath::new(source))
            .is_some(),
        "restored metadata must carry the compiled source's hash ({} entries)",
        restored.len()
    );
}

async fn compile_once(service: &ZccacheService, request: CompileRequest) {
    let response = service.compile(request).await.expect("compile");
    assert_eq!(
        response.exit_code,
        0,
        "compiler stderr: {}",
        String::from_utf8_lossy(&response.stderr)
    );
}

#[tokio::test]
async fn graceful_shutdown_persists_metadata_snapshot() {
    let Some(compiler) = crate::test_support::find_clang() else {
        return;
    };
    let fx = fixture(compiler);
    let service = start_service(&fx.cache_root).await;
    compile_once(&service, fx.request.clone()).await;
    let state_root = service.stats().await.expect("stats").cache_root;
    let report = service
        .shutdown_detailed(ShutdownMode::Graceful)
        .await
        .expect("shutdown");
    assert!(report.flushed.is_complete(), "{report:?}");
    assert_metadata_persisted(state_root.as_path(), &fx.source);

    // The next start must restore it instead of starting empty.
    let restarted = start_service(&fx.cache_root).await;
    let stats = restarted.stats().await.expect("stats");
    assert!(
        stats.metadata_entries > 0,
        "restarted service must restore the persisted metadata snapshot"
    );
    restarted
        .shutdown(ShutdownMode::Graceful)
        .await
        .expect("restart shutdown");
}

#[tokio::test]
async fn drop_without_shutdown_persists_metadata_snapshot() {
    let Some(compiler) = crate::test_support::find_clang() else {
        return;
    };
    let fx = fixture(compiler);
    let state_root;
    {
        let service = start_service(&fx.cache_root).await;
        compile_once(&service, fx.request.clone()).await;
        state_root = service.stats().await.expect("stats").cache_root;
        drop(service);
    }
    assert_metadata_persisted(state_root.as_path(), &fx.source);
}

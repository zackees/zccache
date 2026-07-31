//! #1157 finding 2 — what a depgraph reset really costs, end to end.
//!
//! The issue assumed on-disk artifacts "remain hittable in principle" after
//! the graph is dropped. They do not: an artifact key is
//! `H(logical_context_key, sorted(path → content_hash))` over the source
//! *plus every resolved include*, and that include set lives only in the
//! depgraph — `zccache_artifact::ArtifactIndex` records outputs, stdout,
//! stderr and exit code, and nothing about the inputs. So an empty graph
//! forces `CacheVerdict::Cold` and one real recompile per translation unit.
//!
//! What survives is the *store*: the recompile recomputes the identical key
//! and re-adopts the artifact, so the reset costs one recompile, not a cache
//! wipe. Both halves are pinned below, because "we recompile once" and "we
//! lost the cache" are very different incidents and the difference was
//! previously untested.
//!
//! The third test covers the case the quarantine sidecar exists for: a cache
//! root shared by two binaries with different `DEPGRAPH_VERSION`s. That used
//! to destroy the other side's snapshot on every switch; now each side keeps
//! its own and stays warm.

use std::path::Path;

use super::super::*;
use super::multi_restart_context_key::{
    quiesce_and_persist, spawn_index_writer, write_fake_multi_cc,
};
use super::CacheDirEnvGuard;

/// Rewrite the LE `u32` schema tag at bytes 4..8 (immediately after the
/// 4-byte magic) so `classify_load` reports `VersionMismatch` — the exact
/// shape a real schema bump produces, without having to bump the constant.
fn skew_snapshot_version(path: &Path) {
    let mut bytes = std::fs::read(path).expect("a persisted snapshot must exist");
    bytes[4..8].copy_from_slice(&(crate::depgraph::DEPGRAPH_VERSION + 1).to_le_bytes());
    std::fs::write(path, &bytes).unwrap();
}

/// Install the startup decision the daemon really makes, rather than a
/// test-local approximation: `daemon::entry` calls exactly this and installs
/// exactly this graph.
fn start_with_production_load(server: &DaemonServer, depgraph_path: &Path) -> bool {
    let load = crate::daemon::depgraph_load::load_for_startup(depgraph_path);
    let warm = load.graph.is_some();
    server.dep_graph_setter().install(load.graph, load.warning);
    warm
}

struct Fixture {
    _tmp: tempfile::TempDir,
    cache_root: crate::core::NormalizedPath,
    depgraph_path: crate::core::NormalizedPath,
    cc: std::path::PathBuf,
    work: std::path::PathBuf,
    out: std::path::PathBuf,
    args: Vec<String>,
}

impl Fixture {
    fn new(stem: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let cache_root: crate::core::NormalizedPath = tmp.path().join("zccache-cache").into();
        let depgraph_path =
            crate::core::config::depgraph_dir_from_cache_dir(&cache_root).join("depgraph.bin");
        let cc = write_fake_multi_cc(tmp.path());
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let src = work.join(format!("{stem}.c"));
        std::fs::write(&src, format!("int {stem}(void) {{ return 7; }}\n")).unwrap();
        let out = work.join(format!("{stem}.o"));
        let args = vec!["-c".to_string(), src.to_string_lossy().into_owned()];
        Self {
            _tmp: tmp,
            cache_root,
            depgraph_path,
            cc,
            work,
            out,
            args,
        }
    }

    fn bind(&self) -> DaemonServer {
        DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &self.cache_root)
            .unwrap()
    }

    async fn compile(&self, server: &DaemonServer) -> bool {
        let response = handle_compile_ephemeral(
            &server.state,
            std::process::id(),
            &self.work,
            &self.cc,
            &self.args,
            &self.work,
            None,
            Vec::new(),
        )
        .await;
        match response {
            Response::CompileResult {
                exit_code, cached, ..
            } => {
                assert_eq!(exit_code, 0, "the fake compiler must succeed");
                cached
            }
            other => panic!("expected CompileResult, got {other:?}"),
        }
    }

    /// Run one full daemon lifetime: bind, load the graph the production way,
    /// compile, then perform the real shutdown durability drain.
    async fn session(&self, load_graph: bool) -> bool {
        let mut server = self.bind();
        let writer = spawn_index_writer(&mut server);
        if load_graph {
            start_with_production_load(&server, self.depgraph_path.as_path());
        }
        let cached = self.compile(&server).await;
        quiesce_and_persist(&server, writer, self.depgraph_path.as_path()).await;
        cached
    }
}

/// The honest blast radius of a schema bump: exactly one recompile per
/// translation unit, and the artifact is re-adopted rather than lost.
///
/// If this ever flips to `cached == true` on the reset build, the premise
/// behind #1157 finding 2 changed and the module docs above are stale.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_version_reset_costs_one_recompile_and_leaves_the_artifact_re_adoptable() {
    let fixture = Fixture::new("reset_probe");
    let _env_lock = CacheDirEnvGuard::lock();

    assert!(
        !fixture.session(false).await,
        "the very first compile is a genuine cold miss"
    );
    let cold_bytes = std::fs::read(&fixture.out).unwrap();
    std::fs::remove_file(&fixture.out).unwrap();

    // Control: with the snapshot intact the next session is warm. Without
    // this, the reset assertion below could pass for an unrelated reason.
    assert!(
        fixture.session(true).await,
        "an intact snapshot must still hit — otherwise the reset assertion \
         below proves nothing"
    );
    std::fs::remove_file(&fixture.out).unwrap();

    // A schema bump: same bytes, foreign version tag.
    skew_snapshot_version(fixture.depgraph_path.as_path());

    let server = fixture.bind();
    assert!(
        !start_with_production_load(&server, fixture.depgraph_path.as_path()),
        "a foreign schema version must not be installed"
    );
    assert!(
        !fixture.compile(&server).await,
        "the include set an artifact key is built from lives only in the \
         depgraph, so an empty graph really does force a recompile — this is \
         the cost #1157 describes, and it cannot be avoided without either a \
         versioned migration or reinterpreting foreign-schema bytes"
    );
    assert_eq!(
        std::fs::read(&fixture.out).unwrap(),
        cold_bytes,
        "the recompile must reproduce the same object"
    );
    let mut server = server;
    let writer = spawn_index_writer(&mut server);
    quiesce_and_persist(&server, writer, fixture.depgraph_path.as_path()).await;
    drop(server);
    std::fs::remove_file(&fixture.out).unwrap();

    assert!(
        fixture.session(true).await,
        "the price of a reset is ONE recompile, not a poisoned cache: the \
         recompile recomputes the identical artifact key and re-adopts the \
         artifact that was on disk all along"
    );
    assert_eq!(std::fs::read(&fixture.out).unwrap(), cold_bytes);
}

/// A cache root shared by two binaries with different `DEPGRAPH_VERSION`s
/// used to have each side destroy the other's snapshot on every switch, so
/// both cold-recompiled the world forever. Each side now keeps its own
/// version-tagged sidecar and comes back warm.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn an_oscillating_schema_version_stays_warm_through_the_quarantine_sidecar() {
    let fixture = Fixture::new("oscillate");
    let _env_lock = CacheDirEnvGuard::lock();

    assert!(
        !fixture.session(false).await,
        "first compile is a cold miss"
    );
    let cold_bytes = std::fs::read(&fixture.out).unwrap();
    std::fs::remove_file(&fixture.out).unwrap();

    // Stand in for "the other binary ran here": it rejected our snapshot,
    // parked it under our version tag, and left its own snapshot as primary.
    let ours = crate::depgraph::quarantine::quarantine_path(
        fixture.depgraph_path.as_path(),
        crate::depgraph::DEPGRAPH_VERSION,
    );
    std::fs::rename(fixture.depgraph_path.as_path(), ours.as_path()).unwrap();
    std::fs::copy(ours.as_path(), fixture.depgraph_path.as_path()).unwrap();
    skew_snapshot_version(fixture.depgraph_path.as_path());

    let server = fixture.bind();
    assert!(
        start_with_production_load(&server, fixture.depgraph_path.as_path()),
        "our own parked snapshot carries this build's exact schema version and \
         passes the same magic/version/rkyv validation the primary gets"
    );
    assert!(
        fixture.compile(&server).await,
        "recovering the sidecar must actually restore hits — a reset that only \
         preserves bytes nobody reads back is not a fix"
    );
    assert_eq!(std::fs::read(&fixture.out).unwrap(), cold_bytes);

    // The foreign snapshot was preserved rather than clobbered, so the other
    // binary comes back warm too.
    let theirs = crate::depgraph::quarantine::quarantine_path(
        fixture.depgraph_path.as_path(),
        crate::depgraph::DEPGRAPH_VERSION + 1,
    );
    assert!(
        theirs.as_path().exists(),
        "the rejected snapshot must be moved aside, not left for this \
         daemon's graceful shutdown to overwrite"
    );
}

/// Restart-loop regression (#1157's own test list): repeating the reset must
/// be idempotent — no accumulation, no second-order failure.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn repeated_resets_are_idempotent() {
    let fixture = Fixture::new("idempotent");
    let _env_lock = CacheDirEnvGuard::lock();

    assert!(!fixture.session(false).await);
    std::fs::remove_file(&fixture.out).unwrap();

    for round in 0..3 {
        skew_snapshot_version(fixture.depgraph_path.as_path());
        let server = fixture.bind();
        assert!(!start_with_production_load(
            &server,
            fixture.depgraph_path.as_path()
        ));
        let cached = fixture.compile(&server).await;
        assert!(!cached, "round {round}: a reset session recompiles");
        let mut server = server;
        let writer = spawn_index_writer(&mut server);
        quiesce_and_persist(&server, writer, fixture.depgraph_path.as_path()).await;
        drop(server);
        std::fs::remove_file(&fixture.out).unwrap();

        assert!(
            fixture.session(true).await,
            "round {round}: the session after a reset must be warm again"
        );
        std::fs::remove_file(&fixture.out).unwrap();
    }

    // Quarantine is capped: repeated resets reuse the same version-tagged
    // sidecar rather than piling up snapshots in the depgraph directory.
    let sidecars = std::fs::read_dir(
        crate::core::config::depgraph_dir_from_cache_dir(&fixture.cache_root).as_path(),
    )
    .unwrap()
    .flatten()
    .filter(|entry| {
        entry
            .file_name()
            .to_string_lossy()
            .starts_with("depgraph.v")
    })
    .count();
    assert_eq!(
        sidecars, 1,
        "three resets of the same foreign version must leave one sidecar"
    );
}

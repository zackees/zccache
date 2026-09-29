//! `ZCCACHE_MODE=LINK` governs C/C++ compile hits, not only rustc (#1764).
//!
//! An object or PCH is `AtomicReplaceOnly`: the compiler replaces the path and
//! nothing edits it in place, so only an explicit LINK opts it into a shared
//! inode. AUTO (and every other mode) keeps the conservative reflink-else-copy
//! delivery; depfiles and unknown outputs stay independent in every mode.

use super::*;

const OBJECT: &[u8] = b"compiled object bytes";

fn same_file(a: &Path, b: &Path) -> bool {
    crate::platform::fs::identity::same_file(a, b).unwrap_or(false)
}

struct Hit {
    dir: tempfile::TempDir,
    server: crate::daemon::server::DaemonServer,
    sid: SessionId,
    blob: NormalizedPath,
    depfile_blob: NormalizedPath,
}

impl Hit {
    /// A cached artifact `[<object>, <depfile>]` whose payloads are file blobs.
    fn new(object_name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let server = crate::daemon::server::tests::bind_isolated_server(dir.path());
        let state = server.state.as_ref();
        let blob = state.artifact_dir.join("artifact-key_0");
        let depfile_blob = state.artifact_dir.join("artifact-key_1");
        let _ = crate::platform::fs::permissions::make_writable(&blob);
        std::fs::write(&blob, OBJECT).unwrap();
        write_authoritative_blob_digest(&blob).unwrap();
        std::fs::write(&depfile_blob, b"out.o: in.cc\n").unwrap();
        write_authoritative_blob_digest(&depfile_blob).unwrap();
        let meta = ArtifactIndex::new(
            vec![object_name.to_string(), "out.d".to_string()],
            vec![OBJECT.len() as u64, 13],
            Arc::new(Vec::new()),
            Arc::new(Vec::new()),
            0,
        );
        state.artifacts.insert(
            "artifact-key".to_string(),
            CachedArtifact::from_file_payloads(meta, vec![blob.clone(), depfile_blob.clone()]),
        );
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
        Self {
            dir,
            server,
            sid,
            blob,
            depfile_blob,
        }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Replay the hit as a C/C++ request (`rustc_archive_hardlink_eligible`
    /// is `None`) writing `output` and the depfile `depfile`.
    fn replay(&self, output: &Path, depfile: &Path, mode: MaterializationMode) {
        let source: NormalizedPath = self.root().join("in.cc").into();
        let output: NormalizedPath = output.into();
        let response = materialize_cached_compile_hit(CachedHitMaterializeRequest {
            state: self.server.state.as_ref(),
            sid: &self.sid,
            artifact_key_hex: "artifact-key",
            verdict_key_hex: None,
            source_path: &source,
            output_path: &output,
            secondary_output_dir: self.root().into(),
            current_depfile_dest: Some(depfile.into()),
            current_rustc_out_dir: None,
            depfile_key_root: None,
            compile_start: Instant::now(),
            hit_label: "HIT_TEST",
            cached_error_label: "CACHED_ERROR_TEST",
            record_compilation: false,
            downgrade_output_metadata: false,
            mtime_floor_paths: Vec::new(),
            rustc_metadata_compat_outputs: None,
            rustc_archive_hardlink_eligible: None,
            materialization_mode: mode,
            phases: CachedHitPhases::request_cache(0, 0),
        })
        .unwrap();
        assert!(matches!(
            response,
            Response::CompileResult {
                exit_code: 0,
                cached: true,
                ..
            }
        ));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn link_mode_hardlinks_a_c_object_and_keeps_the_depfile_independent() {
    for name in ["out.o", "out.obj", "out.pch", "out.gch", "out.pcm"] {
        let hit = Hit::new(name);
        let out = hit.root().join(name);
        let depfile = hit.root().join("out.d");
        hit.replay(&out, &depfile, MaterializationMode::Link);
        assert_eq!(std::fs::read(&out).unwrap(), OBJECT);
        assert!(
            same_file(&out, &hit.blob),
            "LINK must hardlink the cached {name} on a hit (#1764)"
        );
        assert!(
            !same_file(&depfile, &hit.depfile_blob),
            "a depfile may be edited in place and must stay independent"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn every_other_mode_keeps_c_objects_independent() {
    for mode in MaterializationMode::ALL {
        if mode == MaterializationMode::Link {
            continue;
        }
        let hit = Hit::new("out.o");
        let out = hit.root().join("out.o");
        hit.replay(&out, &hit.root().join("out.d"), mode);
        assert_eq!(std::fs::read(&out).unwrap(), OBJECT);
        assert!(
            !same_file(&out, &hit.blob),
            "{mode:?} must not hardlink a C object; only LINK opts in (#1764)"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_c_outputs_stay_independent_under_link() {
    let hit = Hit::new("out.bin");
    let out = hit.root().join("out.bin");
    hit.replay(&out, &hit.root().join("out.d"), MaterializationMode::Link);
    assert!(!same_file(&out, &hit.blob));
}

#[tokio::test(flavor = "current_thread")]
async fn a_compile_rewriting_a_hardlinked_object_leaves_the_cache_blob_unchanged() {
    let hit = Hit::new("out.o");
    let out = hit.root().join("out.o");
    hit.replay(&out, &hit.root().join("out.d"), MaterializationMode::Link);
    assert!(same_file(&out, &hit.blob));

    // The pre-compile step the miss path runs for every declared output,
    // then the compiler writing the new object in place.
    break_output_hardlink_before_compile(&out).unwrap();
    assert!(!same_file(&out, &hit.blob), "the output must be detached");
    std::fs::write(&out, b"a different object").unwrap();

    assert_eq!(std::fs::read(&hit.blob).unwrap(), OBJECT);
    // The hit replays the same bytes again afterwards.
    hit.replay(&out, &hit.root().join("out.d"), MaterializationMode::Link);
    assert_eq!(std::fs::read(&out).unwrap(), OBJECT);
}

/// Perf budget (#1764): a LINK hit of a large object moves no bytes and stays
/// far under the cost of copying it.
#[tokio::test(flavor = "current_thread")]
async fn link_hit_delivery_copies_no_bytes_within_budget() {
    let hit = Hit::new("out.o");
    let big = vec![0xA5u8; 8 << 20];
    let _ = crate::platform::fs::permissions::make_writable(&hit.blob);
    std::fs::write(&hit.blob, &big).unwrap();
    write_authoritative_blob_digest(&hit.blob).unwrap();

    // Verification hashes the whole blob; in a debug test build on a slow
    // runner that alone can exceed the budget (345 ms on macOS, #1823). It is
    // identical in every mode, so verify outside the timer and budget only
    // the delivery that LINK changes.
    verify_registered_blob(&hit.blob).unwrap();
    let start = Instant::now();
    let stats = materialize_verified_cached_file_observed(
        &hit.root().join("out.o"),
        &hit.blob,
        crate::compiler::DeliveryPolicy::HardlinkEligible,
        MaterializationMode::Link,
        true,
    )
    .unwrap();
    let elapsed = start.elapsed();
    assert_eq!(stats.copy_bytes, 0, "LINK must not copy the object");
    assert_eq!(stats.hardlink_count, 1);
    assert!(
        elapsed < std::time::Duration::from_millis(250),
        "LINK delivery of an 8 MiB object took {elapsed:?}"
    );
}

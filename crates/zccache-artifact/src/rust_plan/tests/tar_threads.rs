//! rust-plan tar thread resolver (issue #177) and the parallel-bundling
//! equivalence property: thread count must not change the manifest.

use super::super::*;
use super::{sample_plan, synthetic_target};
use std::sync::{Mutex, MutexGuard, OnceLock};

const THREADS_ENV: &str = "ZCCACHE_RUST_PLAN_TAR_THREADS";

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

struct ThreadsEnvGuard {
    _lock: MutexGuard<'static, ()>,
    previous: Option<std::ffi::OsString>,
}

impl ThreadsEnvGuard {
    fn set(threads: &str) -> Self {
        let lock = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os(THREADS_ENV);
        std::env::set_var(THREADS_ENV, threads);
        Self {
            _lock: lock,
            previous,
        }
    }
}

impl Drop for ThreadsEnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(THREADS_ENV, value),
            None => std::env::remove_var(THREADS_ENV),
        }
    }
}

#[test]
fn tar_threads_parser_accepts_grammar_from_soldr_273() {
    // unset / auto / empty / whitespace -> default (vCPU-bounded, capped at 8)
    let default = default_rust_plan_tar_threads();
    assert!((1..=DEFAULT_RUST_PLAN_TAR_THREADS_CAP).contains(&default));
    assert_eq!(parse_rust_plan_tar_threads(None), default);
    assert_eq!(parse_rust_plan_tar_threads(Some("auto")), default);
    assert_eq!(parse_rust_plan_tar_threads(Some("AUTO")), default);
    assert_eq!(parse_rust_plan_tar_threads(Some("")), default);
    assert_eq!(parse_rust_plan_tar_threads(Some("   ")), default);

    // 1 -> sequential escape hatch
    assert_eq!(parse_rust_plan_tar_threads(Some("1")), 1);

    // Positive integer -> clamped to MAX_RUST_PLAN_TAR_THREADS
    assert_eq!(parse_rust_plan_tar_threads(Some("4")), 4);
    assert_eq!(
        parse_rust_plan_tar_threads(Some("9999")),
        MAX_RUST_PLAN_TAR_THREADS
    );

    // Garbage / 0 -> default (defensive)
    assert_eq!(parse_rust_plan_tar_threads(Some("0")), default);
    assert_eq!(parse_rust_plan_tar_threads(Some("not-a-number")), default);
    assert_eq!(parse_rust_plan_tar_threads(Some("-1")), default);
}

#[test]
fn parallel_bundling_matches_sequential_byte_for_byte() {
    // `select_artifacts` pre-sorts by relative_path; with rayon's ordered
    // `par_iter().collect()` we must end up with the same artifact list,
    // same hashes, same sizes -- regardless of thread count.
    fn bundle_with(threads: usize) -> Vec<RustBundledArtifact> {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target(dir.path());
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);

        let mut candidates = Vec::new();
        collect_files(plan.target_dir.as_path(), &mut candidates).unwrap();
        candidates.sort();
        let mut summary = RustPlanSummary::new(
            RustPlanOperation::Save,
            plan.mode,
            plan.schema_version,
            plan.cache_schema_version,
            rust_plan_cache_key(&plan),
            None,
            None,
        );
        let selected = select_artifacts(&plan, candidates, &mut summary);

        let files_dir = dir.path().join("out").join(format!("t{threads}"));
        std::fs::create_dir_all(&files_dir).unwrap();
        bundle_selected_artifacts_with_threads(&selected, &files_dir, threads).unwrap()
    }

    let sequential = bundle_with(1);
    let parallel = bundle_with(4);

    assert!(!sequential.is_empty());
    assert_eq!(sequential.len(), parallel.len());
    for (seq, par) in sequential.iter().zip(parallel.iter()) {
        assert_eq!(seq.relative_path, par.relative_path);
        assert_eq!(seq.size, par.size);
        assert_eq!(seq.content_hash, par.content_hash);
        assert_eq!(seq.class, par.class);
    }
}

#[test]
fn parallel_bundle_payloads_restore_correctly() {
    let _threads = ThreadsEnvGuard::set("4");
    let dir = tempfile::tempdir().unwrap();
    synthetic_target(dir.path());
    let plan = sample_plan(dir.path(), RustPlanMode::Thin);
    let cache = dir.path().join("cache");

    save_rust_plan_local(&plan, &cache).unwrap();

    std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
    let restored = restore_rust_plan_local(&plan, &cache).unwrap();

    assert_eq!(restored.restored_file_count, 6);
    assert_eq!(
        std::fs::read(plan.target_dir.join("debug/deps/libserde-abc.rlib")).unwrap(),
        b"serde rlib"
    );
    assert_eq!(
        std::fs::read(
            plan.target_dir
                .join("debug/.fingerprint/serde-abc/dep-lib-serde")
        )
        .unwrap(),
        b"fingerprint"
    );
}

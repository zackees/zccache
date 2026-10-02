//! Request-scoped rustc `--test` harness admission through the public
//! embedded API (zccache#1550). Every test drives one already-running
//! service with real rustc compiles; none mutates the process environment.

use super::super::*;
use super::{CompileObservation, CompileOptions};
use tempfile::TempDir;

async fn start_service(temp: &TempDir) -> ZccacheService {
    let mut audit = AuditConfig::default();
    audit.mode = crate::audit::AuditMode::Off;
    ZccacheService::start(ZccacheConfig {
        host: HostIdentity {
            product: "harness-test".into(),
            instance_id: "harness-instance".into(),
            workspace_id: "harness-workspace".into(),
        },
        cache_root: temp.path().join("cache").into(),
        audit,
        limits: ServiceLimits::default(),
        runtime: RuntimeHooks::default(),
        cancellation: None,
    })
    .await
    .expect("service start")
}

const SOURCE: &str = "pub fn answer() -> u32 { 42 }\n\
    #[cfg(test)]\nmod tests {\n    #[test]\n    fn answers() { assert_eq!(super::answer(), 42); }\n}\n";

/// A temp workspace holding `harness.rs`, plus the request builder.
struct Workspace {
    temp: TempDir,
    rustc: crate::core::NormalizedPath,
}

impl Workspace {
    fn new() -> Option<Self> {
        let rustc = crate::test_support::find_rustc()?;
        let temp = TempDir::new().expect("tempdir");
        std::fs::write(temp.path().join("harness.rs"), SOURCE).expect("source");
        std::fs::create_dir_all(temp.path().join("out")).expect("out dir");
        Some(Self { temp, rustc })
    }

    /// Cargo's shape for a unit-test harness (`--test`, no `--crate-type`),
    /// with `extra` appended before the source file.
    fn harness(&self, extra: &[&str], env: &[(&str, &str)]) -> CompileRequest {
        let mut args = vec![
            "--crate-name",
            "harness",
            "--edition=2021",
            "--emit=dep-info,link",
            "--test",
            "-C",
            "metadata=1550",
            "-C",
            "extra-filename=-1550",
        ];
        args.extend_from_slice(extra);
        self.request(&args, env)
    }

    fn request(&self, args: &[&str], env: &[(&str, &str)]) -> CompileRequest {
        let mut args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
        args.push("--out-dir".into());
        args.push(self.temp.path().join("out").to_string_lossy().into_owned());
        args.push(
            self.temp
                .path()
                .join("harness.rs")
                .to_string_lossy()
                .into_owned(),
        );
        CompileRequest {
            audit: AuditContext::new(
                crate::audit::AuditId::new("harness-run").expect("id"),
                crate::audit::AuditId::new("harness-trace").expect("id"),
            ),
            compiler: self.rustc.clone(),
            args,
            cwd: self.temp.path().into(),
            // rustc finds its linker on the request's PATH.
            env: std::env::var("PATH")
                .ok()
                .map(|path| ("PATH".to_string(), path))
                .into_iter()
                .chain(
                    env.iter()
                        .map(|(key, value)| ((*key).to_string(), (*value).to_string())),
                )
                .collect(),
            stdin: Vec::new(),
        }
    }
}

const SHARED_ONLY: CompileOptions =
    CompileOptions::new().with_test_harness_admission(TestHarnessAdmission::SharedOnly);
const ALL: CompileOptions =
    CompileOptions::new().with_test_harness_admission(TestHarnessAdmission::All);

async fn observe(
    service: &ZccacheService,
    request: CompileRequest,
    options: CompileOptions,
) -> CompileObservation {
    let observed = service
        .compile_with_options(request, options)
        .await
        .expect("compile");
    assert_eq!(
        observed.response.exit_code,
        0,
        "rustc failed: {}",
        String::from_utf8_lossy(&observed.response.stderr)
    );
    observed.observation
}

fn assert_observed(
    observation: CompileObservation,
    outcome: CacheOutcome,
    admission: AdmissionDisposition,
    reason: AdmissionReason,
) {
    assert_eq!(
        (
            observation.cache_outcome,
            observation.admission,
            observation.reason
        ),
        (outcome, admission, reason),
        "{observation:?}"
    );
    assert!(
        observation
            .logical_artifact_bytes
            .is_some_and(|bytes| bytes > 0),
        "a harness observation measures its artifact bytes: {observation:?}"
    );
}

#[tokio::test]
async fn request_scoped_admission_alternates_on_one_running_service() {
    let Some(workspace) = Workspace::new() else {
        eprintln!("SKIP request_scoped_admission_alternates_on_one_running_service: no rustc");
        return;
    };
    let service = start_service(&workspace.temp).await;
    use AdmissionDisposition::{Admitted, Skipped};
    use AdmissionReason::RustcTestHarness;

    // shared-only: an identical harness runs and misses every time.
    for _ in 0..2 {
        let observation = observe(&service, workspace.harness(&[], &[]), SHARED_ONLY).await;
        assert_observed(observation, CacheOutcome::Miss, Skipped, RustcTestHarness);
    }

    // all: miss, then the identical request hits with the same byte total.
    let miss = observe(&service, workspace.harness(&[], &[]), ALL).await;
    assert_observed(miss, CacheOutcome::Miss, Admitted, RustcTestHarness);
    let hit = observe(&service, workspace.harness(&[], &[]), ALL).await;
    assert_observed(hit, CacheOutcome::Hit, Admitted, RustcTestHarness);
    assert_eq!(hit.logical_artifact_bytes, miss.logical_artifact_bytes);

    // shared-only again: the stored harness is neither served nor restored.
    let observation = observe(&service, workspace.harness(&[], &[]), SHARED_ONLY).await;
    assert_observed(observation, CacheOutcome::Miss, Skipped, RustcTestHarness);

    // No option: the request's forwarded ZCCACHE_CACHE_TEST_BINS decides,
    // without touching the service process's environment.
    let admit_env = [(crate::core::config::CACHE_TEST_BINS_ENV, "1")];
    let observation = observe(
        &service,
        workspace.harness(&[], &admit_env),
        CompileOptions::default(),
    )
    .await;
    assert_observed(observation, CacheOutcome::Hit, Admitted, RustcTestHarness);
    let refuse_env = [(crate::core::config::CACHE_TEST_BINS_ENV, "0")];
    let observation = observe(
        &service,
        workspace.harness(&[], &refuse_env),
        CompileOptions::default(),
    )
    .await;
    assert_observed(observation, CacheOutcome::Miss, Skipped, RustcTestHarness);

    service
        .shutdown(ShutdownMode::Graceful)
        .await
        .expect("shutdown");
}

#[tokio::test]
async fn ordinary_library_units_stay_cacheable_under_either_policy() {
    let Some(workspace) = Workspace::new() else {
        eprintln!("SKIP ordinary_library_units_stay_cacheable_under_either_policy: no rustc");
        return;
    };
    let service = start_service(&workspace.temp).await;
    let lib = [
        "--crate-name",
        "harness",
        "--edition=2021",
        "--crate-type=rlib",
        "--emit=metadata,link",
    ];
    let miss = observe(&service, workspace.request(&lib, &[]), SHARED_ONLY).await;
    let hit = observe(&service, workspace.request(&lib, &[]), ALL).await;
    for (observation, outcome) in [(miss, CacheOutcome::Miss), (hit, CacheOutcome::Hit)] {
        assert_observed(
            observation,
            outcome,
            AdmissionDisposition::Admitted,
            AdmissionReason::Cacheable,
        );
    }
    service
        .shutdown(ShutdownMode::Graceful)
        .await
        .expect("shutdown");
}

/// Each variant changes exactly one input an admitted harness's bytes depend
/// on, so each must miss rather than be served the baseline's executable.
#[tokio::test]
async fn an_admitted_harness_misses_when_any_input_changes() {
    let Some(workspace) = Workspace::new() else {
        eprintln!("SKIP an_admitted_harness_misses_when_any_input_changes: no rustc");
        return;
    };
    let service = start_service(&workspace.temp).await;
    let baseline = observe(&service, workspace.harness(&[], &[]), ALL).await;
    assert_eq!(baseline.cache_outcome, CacheOutcome::Miss);
    let repeat = observe(&service, workspace.harness(&[], &[]), ALL).await;
    assert_eq!(
        repeat.cache_outcome,
        CacheOutcome::Hit,
        "control: {repeat:?}"
    );

    let mut variants: Vec<(&str, Vec<&str>)> = vec![
        ("codegen option", vec!["-C", "opt-level=1"]),
        ("cfg", vec!["--cfg", "feature=\"extra\""]),
        ("debuginfo", vec!["-C", "debuginfo=1"]),
    ];
    // Linker inputs: the link arguments shape the executable's bytes.
    if cfg!(target_os = "linux") {
        variants.push(("link arg", vec!["-C", "link-arg=-Wl,--build-id=sha1"]));
    }
    for (what, extra) in variants {
        let observation = observe(&service, workspace.harness(&extra, &[]), ALL).await;
        assert_eq!(
            observation.cache_outcome,
            CacheOutcome::Miss,
            "a changed {what} must not be served the baseline harness: {observation:?}"
        );
    }

    // The same source without `--test` (no cfg(test), no harness) is a
    // different product.
    let bin = [
        "--crate-name",
        "harness",
        "--edition=2021",
        "--crate-type=lib",
        "--emit=dep-info,link",
        "-C",
        "metadata=1550",
        "-C",
        "extra-filename=-1550",
    ];
    let observation = observe(&service, workspace.request(&bin, &[]), ALL).await;
    assert_eq!(
        observation.cache_outcome,
        CacheOutcome::Miss,
        "{observation:?}"
    );
    assert_eq!(observation.reason, AdmissionReason::Cacheable);

    // A source edit misses too.
    std::fs::write(
        workspace.temp.path().join("harness.rs"),
        SOURCE.replace("42", "43"),
    )
    .expect("edit");
    let observation = observe(&service, workspace.harness(&[], &[]), ALL).await;
    assert_eq!(
        observation.cache_outcome,
        CacheOutcome::Miss,
        "{observation:?}"
    );

    service
        .shutdown(ShutdownMode::Graceful)
        .await
        .expect("shutdown");
}

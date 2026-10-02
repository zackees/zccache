//! Request-scoped rustc `--test` harness admission and its observation
//! (zccache#1550).
//!
//! [`scope`] wraps one embedded compile: it carries the host's explicit
//! [`TestHarnessAdmission`] option into the pipeline and collects what the
//! pipeline decided. Pipeline sites call [`for_request`] to resolve the policy
//! (explicit option, then the request's forwarded environment, then the
//! service default) and the `record_*` / `observe_*` seams to report the
//! decision. Outside a scope — the IPC wrapper path — the explicit option is
//! absent and every record is a no-op, so that path resolves from the client
//! environment alone. Like `inner_trace`, a task-local avoids threading one
//! request option through every pipeline signature and test call site.
//!
//! Admission never mutates process or service state: two requests with
//! different policies can run concurrently against one service.

use std::cell::Cell;
use std::path::Path;

use crate::compiler::{CacheableCompilation, ParsedInvocation};
use crate::core::config::{TestHarnessAdmission, MAX_ADMITTED_TEST_HARNESS_BYTES};

/// Whether a compile's artifacts were admitted to the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdmissionDisposition {
    /// The artifacts were looked up and stored (or served from the cache).
    Admitted,
    /// The compile ran directly; nothing was stored.
    Skipped,
}

impl AdmissionDisposition {
    /// Stable lowercase label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admitted => "admitted",
            Self::Skipped => "skipped",
        }
    }
}

/// Stable categorical reason for an [`AdmissionDisposition`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AdmissionReason {
    /// An ordinary cacheable unit (library, build script, proc-macro, bin).
    Cacheable,
    /// A rustc `--test` harness link: admitted under
    /// [`TestHarnessAdmission::All`], skipped under
    /// [`TestHarnessAdmission::SharedOnly`].
    RustcTestHarness,
    /// An admitted `--test` harness whose outputs exceed
    /// [`MAX_ADMITTED_TEST_HARNESS_BYTES`]; executed but not stored.
    RustcTestHarnessOverSizeCap,
    /// Any other invocation the canonical parser refuses.
    NonCacheable,
}

impl AdmissionReason {
    /// Stable snake-case label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cacheable => "cacheable",
            Self::RustcTestHarness => "rustc_test_harness",
            Self::RustcTestHarnessOverSizeCap => "rustc_test_harness_over_size_cap",
            Self::NonCacheable => "non_cacheable",
        }
    }
}

/// What the pipeline recorded for one scoped request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RecordedAdmission {
    /// The admission decision, when a pipeline seam made one.
    pub(crate) decision: Option<(AdmissionDisposition, AdmissionReason)>,
    /// Logical compiler-artifact bytes stored, served, or (for a skipped
    /// harness) produced. `None` when no seam measured them.
    pub(crate) logical_artifact_bytes: Option<u64>,
}

impl RecordedAdmission {
    /// The recorded decision, or the one implied by a request no seam
    /// classified: a parser refusal attributes `uncacheable_input`; every
    /// other request went through ordinary admission.
    pub(crate) fn decision_or_default(
        self,
        attributed_miss_reason: Option<&str>,
    ) -> (AdmissionDisposition, AdmissionReason) {
        self.decision.unwrap_or(
            if attributed_miss_reason
                == Some(crate::daemon::compile_journal::miss_reason::UNCACHEABLE_INPUT)
            {
                (AdmissionDisposition::Skipped, AdmissionReason::NonCacheable)
            } else {
                (AdmissionDisposition::Admitted, AdmissionReason::Cacheable)
            },
        )
    }
}

struct AdmissionSlot {
    requested: Option<TestHarnessAdmission>,
    recorded: Cell<RecordedAdmission>,
}

kernal_api::task_local! {
    static ACTIVE_ADMISSION: AdmissionSlot = ACTIVE_ADMISSION_TLS;
}

/// Run one request under an explicit admission option and collect what the
/// pipeline recorded. The caller heap-pins the large compile future.
pub(crate) async fn scope<F>(
    requested: Option<TestHarnessAdmission>,
    future: std::pin::Pin<Box<F>>,
) -> (F::Output, RecordedAdmission)
where
    F: std::future::Future + ?Sized,
{
    let slot = AdmissionSlot {
        requested,
        recorded: Cell::new(RecordedAdmission::default()),
    };
    ACTIVE_ADMISSION
        .scope(slot, async move {
            let output = future.await;
            let recorded = ACTIVE_ADMISSION.with(|slot| slot.recorded.get());
            (output, recorded)
        })
        .await
}

/// The admission policy for the current request.
pub(super) fn for_request(client_env: Option<&[(String, String)]>) -> TestHarnessAdmission {
    let requested = ACTIVE_ADMISSION
        .try_with(|slot| slot.requested)
        .ok()
        .flatten();
    TestHarnessAdmission::resolve(requested, client_env)
}

fn update(apply: impl FnOnce(&mut RecordedAdmission)) {
    let _ = ACTIVE_ADMISSION.try_with(|slot| {
        let mut recorded = slot.recorded.get();
        apply(&mut recorded);
        slot.recorded.set(recorded);
    });
}

fn record_decision(disposition: AdmissionDisposition, reason: AdmissionReason) {
    update(|recorded| recorded.decision = Some((disposition, reason)));
}

/// True inside a [`scope`] that collects observations.
pub(super) fn observing() -> bool {
    ACTIVE_ADMISSION.try_with(|_| ()).is_ok()
}

/// Record the logical artifact bytes a cache hit materialized.
pub(super) fn record_artifact_bytes(bytes: u64) {
    update(|recorded| recorded.logical_artifact_bytes = Some(bytes));
}

/// True when the canonical parser classifies this request as a rustc
/// `--test` harness, independent of any admission policy.
pub(super) fn is_test_harness_request(compiler: &Path, args: &[String]) -> bool {
    crate::compiler::parse_invocation_with_admission(
        &compiler.to_string_lossy(),
        args,
        TestHarnessAdmission::All,
    )
    .is_rustc_test_harness()
}

/// Record a non-cacheable bypass decided before or beside the parser
/// (Dylint, time macros, ambiguous include discovery, incomplete outputs).
pub(super) fn observe_bypass() {
    record_decision(AdmissionDisposition::Skipped, AdmissionReason::NonCacheable);
}

/// Record the canonical parser's decision for this request: a refusal is
/// skipped, an admitted `--test` harness is marked as such (a later store
/// gate may still bound it), and ordinary admission records nothing.
pub(super) fn observe_parsed(parsed: &ParsedInvocation) {
    match (parsed, parsed.is_rustc_test_harness()) {
        (ParsedInvocation::NonCacheable { .. }, true) => {
            record_decision(
                AdmissionDisposition::Skipped,
                AdmissionReason::RustcTestHarness,
            );
        }
        (ParsedInvocation::NonCacheable { .. }, false) => {
            record_decision(AdmissionDisposition::Skipped, AdmissionReason::NonCacheable);
        }
        (_, true) => record_decision(
            AdmissionDisposition::Admitted,
            AdmissionReason::RustcTestHarness,
        ),
        (_, false) => {}
    }
}

/// Decide whether a successful miss with `logical_artifact_bytes` of planned
/// outputs may be stored, and record the decision.
///
/// Only an admitted `--test` harness is size-bounded; every other unit keeps
/// the store's ordinary admission.
pub(super) fn admit_store(compilation: &CacheableCompilation, logical_artifact_bytes: u64) -> bool {
    let (disposition, reason) = store_decision(
        compilation.is_rustc_test_harness(),
        logical_artifact_bytes,
        MAX_ADMITTED_TEST_HARNESS_BYTES,
    );
    if reason == AdmissionReason::RustcTestHarnessOverSizeCap {
        tracing::info!(
            logical_artifact_bytes,
            cap = MAX_ADMITTED_TEST_HARNESS_BYTES,
            "admitted test harness exceeds the size cap; executed without storing"
        );
    }
    update(|recorded| {
        recorded.decision = Some((disposition, reason));
        recorded.logical_artifact_bytes = Some(logical_artifact_bytes);
    });
    disposition == AdmissionDisposition::Admitted
}

/// The store decision for a successful miss reaching storage.
fn store_decision(
    test_harness: bool,
    logical_artifact_bytes: u64,
    cap: u64,
) -> (AdmissionDisposition, AdmissionReason) {
    match (test_harness, logical_artifact_bytes <= cap) {
        (false, _) => (AdmissionDisposition::Admitted, AdmissionReason::Cacheable),
        (true, true) => (
            AdmissionDisposition::Admitted,
            AdmissionReason::RustcTestHarness,
        ),
        (true, false) => (
            AdmissionDisposition::Skipped,
            AdmissionReason::RustcTestHarnessOverSizeCap,
        ),
    }
}

/// Measure a refused harness's outputs after it ran directly, using the same
/// canonical output plan an admitted harness would have stored. No-op unless
/// a scope is collecting observations.
pub(super) fn observe_skipped_harness_outputs(compiler: &str, args: &[String], cwd: &Path) {
    if !observing() {
        return;
    }
    let parsed =
        crate::compiler::parse_invocation_with_admission(compiler, args, TestHarnessAdmission::All);
    let ParsedInvocation::Cacheable(compilation) = parsed else {
        return;
    };
    let output_path = if compilation.output_file.is_absolute() {
        compilation.output_file.clone()
    } else {
        crate::core::NormalizedPath::new(cwd).join(&compilation.output_file)
    };
    // Same argv selection as the cacheable path in `request_prep`.
    let rustc_argv = crate::compiler::dylint_inner_rustc_args(compiler, args)
        .ok()
        .flatten()
        .map_or(args, |(_, inner)| inner);
    let rustc_args = crate::depgraph::parse_rustc_args(rustc_argv, cwd);
    let bytes = super::rustc::collect_rustc_output_files(&rustc_args, output_path.as_path(), cwd)
        .iter()
        .map(|output| output.size)
        .fold(0_u64, u64::saturating_add);
    record_artifact_bytes(bytes);
}

#[cfg(test)]
#[path = "test_harness_admission_tests.rs"]
mod tests;

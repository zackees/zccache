//! Unit tests for request-scoped `--test` harness admission (zccache#1550).

use super::*;

#[test]
fn only_an_admitted_harness_is_size_bounded() {
    use AdmissionDisposition::{Admitted, Skipped};
    use AdmissionReason::{Cacheable, RustcTestHarness, RustcTestHarnessOverSizeCap};
    assert_eq!(store_decision(false, 10, 5), (Admitted, Cacheable));
    assert_eq!(store_decision(true, 5, 5), (Admitted, RustcTestHarness));
    assert_eq!(
        store_decision(true, 6, 5),
        (Skipped, RustcTestHarnessOverSizeCap)
    );
}

#[test]
fn labels_are_stable() {
    assert_eq!(AdmissionDisposition::Admitted.as_str(), "admitted");
    assert_eq!(AdmissionDisposition::Skipped.as_str(), "skipped");
    assert_eq!(
        AdmissionReason::RustcTestHarness.as_str(),
        "rustc_test_harness"
    );
    assert_eq!(
        AdmissionReason::RustcTestHarnessOverSizeCap.as_str(),
        "rustc_test_harness_over_size_cap"
    );
}

#[tokio::test]
async fn an_explicit_option_reaches_the_pipeline_only_inside_its_scope() {
    let admit_env = vec![(
        crate::core::config::CACHE_TEST_BINS_ENV.to_string(),
        "1".to_string(),
    )];
    let (inside, recorded) = scope(
        Some(TestHarnessAdmission::SharedOnly),
        Box::pin(async { for_request(Some(&admit_env)) }),
    )
    .await;
    assert_eq!(inside, TestHarnessAdmission::SharedOnly);
    assert_eq!(recorded, RecordedAdmission::default());
    // Outside a scope (the IPC wrapper path) the request env decides.
    assert_eq!(for_request(Some(&admit_env)), TestHarnessAdmission::All);
}

#[test]
fn an_unclassified_request_defaults_from_its_miss_attribution() {
    use crate::daemon::compile_journal::miss_reason;
    let none = RecordedAdmission::default();
    assert_eq!(
        none.decision_or_default(Some(miss_reason::UNCACHEABLE_INPUT)),
        (AdmissionDisposition::Skipped, AdmissionReason::NonCacheable)
    );
    assert_eq!(
        none.decision_or_default(Some(miss_reason::CONTEXT_NOT_FOUND)),
        (AdmissionDisposition::Admitted, AdmissionReason::Cacheable)
    );
}

//! Key-composition and exact-exec staging planner tests.
use super::*;
use tempfile::tempdir;

fn h(byte: u8) -> ContentHash {
    ContentHash::from_bytes([byte; 32])
}

fn empty_extra() -> Arc<Vec<u8>> {
    Arc::new(Vec::new())
}

#[test]
fn primary_key_changes_when_input_hash_changes() {
    let k1 = compose_primary_key(
        &h(1),
        &["--json".into()],
        &[("PATH".into(), "/bin".into())],
        Path::new("/p"),
        true,
        &[("src/a.cpp".into(), h(2))],
        &[],
        &[NormalizedPath::from("out.json")],
        &empty_extra(),
    );
    let k2 = compose_primary_key(
        &h(1),
        &["--json".into()],
        &[("PATH".into(), "/bin".into())],
        Path::new("/p"),
        true,
        &[("src/a.cpp".into(), h(3))],
        &[],
        &[NormalizedPath::from("out.json")],
        &empty_extra(),
    );
    assert_ne!(k1, k2);
}

#[test]
fn primary_key_stable_for_env_order() {
    let k1 = compose_primary_key(
        &h(1),
        &[],
        &[
            ("PATH".into(), "/bin".into()),
            ("LINT_VER".into(), "1".into()),
        ],
        Path::new("/p"),
        true,
        &[],
        &[],
        &[],
        &empty_extra(),
    );
    let k2 = compose_primary_key(
        &h(1),
        &[],
        &[
            ("LINT_VER".into(), "1".into()),
            ("PATH".into(), "/bin".into()),
        ],
        Path::new("/p"),
        true,
        &[],
        &[],
        &[],
        &empty_extra(),
    );
    assert_eq!(k1, k2);
}

#[test]
fn full_key_extends_primary_with_depfile_deps() {
    let primary = compose_primary_key(
        &h(1),
        &[],
        &[],
        Path::new("/p"),
        true,
        &[],
        &[],
        &[],
        &empty_extra(),
    );
    let k_no_deps = compose_full_key(&primary, &[]);
    let k_with = compose_full_key(&primary, &[("h.h".into(), h(9))]);
    // Without deps, full key is *not* equal to primary because of the
    // domain tag — but it must differ from a key with deps.
    assert_ne!(k_no_deps, k_with);
}

#[test]
fn full_key_order_independent_for_dep_pairs() {
    let primary = compose_primary_key(
        &h(1),
        &[],
        &[],
        Path::new("/p"),
        true,
        &[],
        &[],
        &[],
        &empty_extra(),
    );
    let a = vec![("a.h".into(), h(2)), ("b.h".into(), h(3))];
    let b = vec![("b.h".into(), h(3)), ("a.h".into(), h(2))];
    assert_eq!(
        compose_full_key(&primary, &a),
        compose_full_key(&primary, &b)
    );
}

#[test]
fn key_args_filter_drops_matching_args() {
    let filtered = apply_key_args_filter(
        &[
            "compile".into(),
            "--verbose".into(),
            "--no-color".into(),
            "src.cpp".into(),
        ],
        &["^--verbose$".into(), "^--no-color$".into()],
    )
    .unwrap();
    assert_eq!(filtered, vec!["compile".to_string(), "src.cpp".to_string()]);
}

#[test]
fn key_args_filter_invalid_regex_errors() {
    let err = apply_key_args_filter(&["a".into()], &["(".into()]).unwrap_err();
    assert!(err.contains('('));
}

#[test]
fn primary_key_differs_when_scan_changes() {
    let k1 = compose_primary_key(
        &h(1),
        &[],
        &[],
        Path::new("/p"),
        true,
        &[],
        &[("hdr.h".into(), h(7))],
        &[],
        &empty_extra(),
    );
    let k2 = compose_primary_key(
        &h(1),
        &[],
        &[],
        Path::new("/p"),
        true,
        &[],
        &[("hdr.h".into(), h(8))],
        &[],
        &empty_extra(),
    );
    assert_ne!(k1, k2);
}

#[test]
fn exact_exec_planner_rejections_have_stable_reasons() {
    if std::env::var_os(crate::daemon::server::persist::STAGED_ARTIFACTS_ENV).is_none() {
        return;
    }
    let temp = tempdir().unwrap();
    assert!(matches!(
        ExecStagedPlan::build(temp.path(), &[], &[], temp.path()),
        StagedPlanOutcome::Unsupported(StagedPlanReason::NoDeclaredOutputs)
    ));

    let first: NormalizedPath = temp.path().join("one/result.bin").into();
    let second: NormalizedPath = temp.path().join("two/result.bin").into();
    assert!(matches!(
        ExecStagedPlan::build(
            temp.path(),
            &[
                first.to_string_lossy().into_owned(),
                second.to_string_lossy().into_owned(),
            ],
            &[first, second],
            temp.path(),
        ),
        StagedPlanOutcome::Unsupported(StagedPlanReason::OutputNameCollision)
    ));

    let output: NormalizedPath = temp.path().join("result.bin").into();
    assert!(matches!(
        ExecStagedPlan::build(
            temp.path(),
            &["--output=result.bin".into()],
            &[output],
            temp.path(),
        ),
        StagedPlanOutcome::Unsupported(StagedPlanReason::OutputNotInArguments)
    ));
}

#[test]
fn exact_exec_disabled_lane_has_stable_reason() {
    if std::env::var_os(crate::daemon::server::persist::STAGED_ARTIFACTS_ENV).is_some() {
        return;
    }
    let temp = tempdir().unwrap();
    let output: NormalizedPath = temp.path().join("result.bin").into();
    assert!(matches!(
        ExecStagedPlan::build(
            temp.path(),
            &[output.to_string_lossy().into_owned()],
            &[output],
            temp.path(),
        ),
        StagedPlanOutcome::Unsupported(StagedPlanReason::LaneDisabled)
    ));
}

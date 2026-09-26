//! #1523: pin which cache files the audit reads to a fixture shared with the
//! Python cleanup helper (`ci/clear_runtime_telemetry.py`).
//!
//! The Integration workflow clears runtime telemetry after seeding the cache
//! and after every harness or intentional-failure step, so the final
//! `audit-logs` run judges only the strict runtime fixture. That only works
//! while the helper deletes exactly the files [`classify_source`] reads. Both
//! this test and `ci/tests/test_clear_runtime_telemetry.py` consume
//! `ci/log_audit_source_fixture.json`, so either side drifting fails a test.

use super::*;

const FIXTURE: &str = include_str!("../../../../ci/log_audit_source_fixture.json");

struct Entry {
    path: String,
    contents: String,
}

fn entries(fixture: &Value, key: &str) -> Vec<Entry> {
    fixture[key]
        .as_array()
        .unwrap_or_else(|| panic!("fixture `{key}` must be an array"))
        .iter()
        .map(|entry| Entry {
            path: entry["path"].as_str().unwrap().to_string(),
            contents: entry["contents"].as_str().unwrap().to_string(),
        })
        .collect()
}

fn materialize(root: &Path, entries: &[Entry]) -> BTreeSet<NormalizedPath> {
    entries
        .iter()
        .map(|entry| {
            let path = root.join(&entry.path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, &entry.contents).unwrap();
            path.into()
        })
        .collect()
}

fn fixture() -> (Vec<Entry>, Vec<Entry>) {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    (
        entries(&fixture, "telemetry"),
        entries(&fixture, "artifacts"),
    )
}

#[test]
fn audit_reads_exactly_the_shared_fixture_telemetry_files() {
    let (telemetry, artifacts) = fixture();
    let root = tempfile::tempdir().unwrap();
    let expected = materialize(root.path(), &telemetry);
    materialize(root.path(), &artifacts);

    let parsed = parse_sources(root.path()).unwrap();
    let read = parsed
        .lines
        .iter()
        .map(|line| line.path.clone())
        .chain(parsed.malformed.iter().map(|row| row.source.clone()))
        .collect::<BTreeSet<_>>();

    assert_eq!(
        read, expected,
        "audit source classification drifted from ci/log_audit_source_fixture.json; \
         update the fixture and ci/clear_runtime_telemetry.py together"
    );
}

#[test]
fn seeded_and_wrapper_contract_telemetry_fail_the_audit_until_cleared() {
    let (telemetry, artifacts) = fixture();
    let root = tempfile::tempdir().unwrap();
    let telemetry_paths = materialize(root.path(), &telemetry);
    let artifact_paths = materialize(root.path(), &artifacts);

    // The audit stays strict: the exact files from Integration run
    // 33069341127 are flagged while they are present.
    let report = audit_cache_root(
        root.path(),
        LogAuditContext::Integration,
        &AuditOptions::default(),
    )
    .unwrap();
    let flagged = report
        .violations
        .iter()
        .map(|violation| (violation.rule_id.0, violation.source.clone()))
        .collect::<BTreeSet<_>>();
    let seeded_journal: NormalizedPath = root.path().join(&telemetry[0].path).into();
    let wrapper_lifecycle: NormalizedPath = root.path().join(&telemetry[1].path).into();
    for expected in [
        ("no-unknown-miss-reason", seeded_journal),
        ("no-unknown-miss-reason", wrapper_lifecycle.clone()),
        ("no-daemon-unavailable", wrapper_lifecycle),
    ] {
        assert!(flagged.contains(&expected), "{}", report.format_human());
    }

    // After the workflow's cleanup removes the telemetry files, the retained
    // cache artifacts alone (whose bytes mimic forbidden rows) must pass.
    for path in &telemetry_paths {
        fs::remove_file(path.as_path()).unwrap();
    }
    let report = audit_cache_root(
        root.path(),
        LogAuditContext::Integration,
        &AuditOptions::default(),
    )
    .unwrap();
    assert!(report.passed(), "{}", report.format_human());
    assert!(artifact_paths.iter().all(|path| path.as_path().is_file()));
}

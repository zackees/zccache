//! Security contract for environment persistence in compile journals.

use serde::Deserialize;

use super::super::{sanitize_journal_env, JournalContext, JournalEntry};
use super::legacy_entry;

#[derive(Debug, Deserialize)]
struct SecurityFixture {
    schema_version: u32,
    input_env: Vec<(String, String)>,
    expected_env: Vec<(String, String)>,
    forbidden_names: Vec<String>,
    forbidden_fragments: Vec<String>,
    legacy_record: serde_json::Value,
}

fn fixture() -> SecurityFixture {
    serde_json::from_str(include_str!("compile_journal_env_security_v1.json"))
        .expect("security fixture must remain valid JSON")
}

#[test]
fn representative_secrets_are_omitted_and_build_diagnostics_remain() {
    let fixture = fixture();
    assert_eq!(fixture.schema_version, 1);

    let sanitized = sanitize_journal_env(Some(fixture.input_env.clone())).unwrap();
    assert_eq!(sanitized.as_slice(), fixture.expected_env);

    let entry = legacy_entry(
        "2026-07-22T00:00:00Z",
        "hit",
        "rustc",
        vec![],
        "/project",
        Some(fixture.input_env.clone()),
        0,
        None,
        1,
    );
    let json = serde_json::to_string(&entry).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let persisted: Vec<(String, String)> = value
        .get("env")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .map(|pair| {
            let pair = pair.as_array().expect("env pair must be an array");
            (
                pair[0].as_str().unwrap().to_owned(),
                pair[1].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    for rejected in fixture
        .input_env
        .iter()
        .filter(|pair| !fixture.expected_env.contains(pair))
    {
        assert!(
            !persisted.contains(rejected),
            "journal JSON retained rejected env pair {rejected:?}: {json}"
        );
    }
    for forbidden_name in fixture.forbidden_names {
        let encoded_name = serde_json::to_string(&forbidden_name).unwrap();
        assert!(
            !json.contains(&encoded_name),
            "journal JSON retained forbidden env name {forbidden_name:?}: {json}"
        );
    }
    for forbidden in fixture.forbidden_fragments {
        assert!(
            !json.contains(&forbidden),
            "journal JSON leaked forbidden fragment {forbidden:?}: {json}"
        );
    }
    for (name, value) in fixture.expected_env {
        assert!(
            json.contains(&name),
            "allowed name {name:?} missing: {json}"
        );
        assert!(
            json.contains(&value),
            "allowed value for {name:?} missing: {json}"
        );
    }

    assert_eq!(fixture.legacy_record["env"].as_array().unwrap().len(), 2);
}

#[test]
fn sanitized_env_type_prevents_raw_entry_serialization() {
    let entry = legacy_entry(
        "2026-07-22T00:00:00Z",
        "hit",
        "rustc",
        vec![],
        "/project",
        Some(vec![(
            "RUSTFLAGS".into(),
            "github_pat_11AA22BB33CC44DD55EE66FF77GG88HH".into(),
        )]),
        0,
        None,
        1,
    );

    // Even the direct JournalEntry test helper must obtain the opaque env
    // value through sanitize_journal_env; raw Vec pairs do not type-check.
    let json = serde_json::to_string(&entry).unwrap();
    assert!(!json.contains("github_pat_"), "secret leaked: {json}");
    assert!(
        !json.contains("\"env\""),
        "empty env must be omitted: {json}"
    );
}

#[test]
fn journal_context_and_entry_builder_store_only_sanitized_env() {
    let raw = Some(vec![
        ("CC".into(), "clang".into()),
        ("GENERIC_PASSWORD".into(), "not-for-disk".into()),
    ]);
    let ctx = JournalContext::new("rustc".into(), vec![], "/project".into(), raw.clone(), None);
    assert_eq!(
        ctx.env.as_ref().unwrap().as_slice(),
        &[("CC".into(), "clang".into())]
    );

    // The env field's opaque type makes direct raw insertion impossible.
    let direct_ctx = JournalContext {
        compiler: "rustc".into(),
        args: vec![],
        cwd: "/project".into(),
        env: sanitize_journal_env(raw),
        session_id: None,
    };
    let entry = JournalEntry::new(direct_ctx, "hit", 0, 1, None);
    assert_eq!(
        entry.env.as_ref().unwrap().as_slice(),
        &[("CC".into(), "clang".into())]
    );
}

#[test]
fn embedded_and_ipc_entry_points_use_the_shared_context_constructor() {
    let connection = include_str!("../../server/connection.rs");
    let embedded = include_str!("../../server/embedded.rs");
    let connection_production = connection.split("#[cfg(test)]").next().unwrap();
    let embedded_production = embedded.split("#[cfg(test)]").next().unwrap();

    assert_eq!(
        connection_production
            .matches("JournalContext::new(")
            .count(),
        3,
        "every ephemeral, link, and session IPC journal entry point must sanitize"
    );
    assert_eq!(
        embedded_production.matches("JournalContext::new(").count(),
        1,
        "the embedded journal entry point must sanitize"
    );
    assert!(!connection_production.contains("JournalContext {"));
    assert!(!embedded_production.contains("JournalContext {"));
    assert_eq!(connection_production.matches("env.clone()").count(), 3);
    assert!(embedded_production.contains("request.env.clone()"));
    assert!(embedded_production.contains("request.env,"));

    let journal_source = include_str!("../mod.rs");
    assert!(journal_source.contains("env: sanitize_journal_env(env)"));
    assert!(journal_source.contains("env: ctx.env"));
    assert!(journal_source.contains("Option<SanitizedJournalEnv>"));
    let sanitizer_source = include_str!("../env.rs");
    assert!(sanitizer_source.contains("pub struct SanitizedJournalEnv(Vec"));
}

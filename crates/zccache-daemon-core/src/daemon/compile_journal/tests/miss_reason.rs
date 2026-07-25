//! Issue #322 — `miss_reason` constants, `MissDiff` serialization, and the
//! end-to-end JSONL miss-reason wiring tests.

use std::fs;
use std::sync::Arc;

use crate::protocol::Response;

use super::super::{
    capture_miss_reason, extract_outcome, miss_reason, record_context_key, record_miss_reason,
    CompileJournal, JournalContext, JournalEntry, MissDiff,
};
use super::wait_for_lines;

// ─── MissDiff serialization ───────────────────────────────────────────────

#[test]
fn miss_diff_omits_empty_arrays() {
    // An entirely empty MissDiff must serialize as `{}` so we never burn
    // bytes on noise.
    let diff = MissDiff {
        changed_files: vec![],
        changed_flags: vec![],
        changed_deps: vec![],
    };
    let json = serde_json::to_string(&diff).unwrap();
    assert_eq!(
        json, "{}",
        "empty MissDiff should serialize as {{}}: {json}"
    );
}

#[test]
fn miss_diff_round_trips_populated_arrays() {
    let diff = MissDiff {
        changed_files: vec!["src/main.rs".to_string(), "src/lib.rs".to_string()],
        changed_flags: vec!["-C".to_string(), "opt-level=3".to_string()],
        changed_deps: vec!["serde@1.0.0".to_string()],
    };
    let json = serde_json::to_string(&diff).unwrap();
    let parsed: MissDiff = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.changed_files, diff.changed_files);
    assert_eq!(parsed.changed_flags, diff.changed_flags);
    assert_eq!(parsed.changed_deps, diff.changed_deps);
}

#[test]
fn miss_diff_partial_population_omits_empty() {
    // Only changed_files populated — the other two arrays must be absent
    // from the JSON (skipped, not emitted as []).
    let diff = MissDiff {
        changed_files: vec!["src/main.rs".to_string()],
        changed_flags: vec![],
        changed_deps: vec![],
    };
    let json = serde_json::to_string(&diff).unwrap();
    assert!(json.contains("\"changed_files\""), "json: {json}");
    assert!(!json.contains("\"changed_flags\""), "json: {json}");
    assert!(!json.contains("\"changed_deps\""), "json: {json}");
}

// ─── miss_reason constants ────────────────────────────────────────────────

#[test]
fn miss_reason_constants_match_documented_schema() {
    // Acceptance criteria #2: the finite enum of miss reasons must be a
    // documented public surface so consumers can build histograms.
    assert_eq!(miss_reason::CONTEXT_NOT_FOUND, "context_not_found");
    assert_eq!(
        miss_reason::INPUT_FINGERPRINT_MISMATCH,
        "input_fingerprint_mismatch"
    );
    assert_eq!(miss_reason::NO_ARTIFACT_FOR_KEY, "no_artifact_for_key");
    assert_eq!(miss_reason::VERSION_SKEW, "version_skew");
    assert_eq!(miss_reason::UNCACHEABLE_INPUT, "uncacheable_input");
    assert_eq!(
        miss_reason::DESTINATION_WRITE_FAILED,
        "destination_write_failed"
    );
    assert_eq!(miss_reason::UNKNOWN, "unknown");
    // The all-values slice lets consumers iterate the closed set.
    assert!(miss_reason::ALL.contains(&miss_reason::UNKNOWN));
    assert!(miss_reason::ALL.contains(&miss_reason::CONTEXT_NOT_FOUND));
}

#[test]
fn every_miss_reason_has_a_production_emitter() {
    let production_sources = [
        include_str!("../../server/connection/mod.rs"),
        include_str!("../../server/connection/dispatch.rs"),
        include_str!("../../server/connection/attribution.rs"),
        include_str!("../../server/compiler_hash.rs"),
        include_str!("../../server/handle_compile/pipeline/mod.rs"),
        include_str!("../../server/handle_compile_multi.rs"),
        include_str!("../outcome.rs"),
    ]
    .into_iter()
    .map(|source| {
        source
            .split("\n#[cfg(test)]\nmod ")
            .next()
            .unwrap_or(source)
    })
    .collect::<String>();
    let emitters = [
        (
            miss_reason::CONTEXT_NOT_FOUND,
            "miss_reason::CONTEXT_NOT_FOUND",
        ),
        (
            miss_reason::INPUT_FINGERPRINT_MISMATCH,
            "miss_reason::INPUT_FINGERPRINT_MISMATCH",
        ),
        (
            miss_reason::NO_ARTIFACT_FOR_KEY,
            "miss_reason::NO_ARTIFACT_FOR_KEY",
        ),
        (miss_reason::VERSION_SKEW, "miss_reason::VERSION_SKEW"),
        (
            miss_reason::UNCACHEABLE_INPUT,
            "miss_reason::UNCACHEABLE_INPUT",
        ),
        (
            miss_reason::DESTINATION_WRITE_FAILED,
            "miss_reason::DESTINATION_WRITE_FAILED",
        ),
        (miss_reason::UNKNOWN, "miss_reason::UNKNOWN"),
    ];
    assert_eq!(emitters.len(), miss_reason::ALL.len());
    for (reason, source_pattern) in emitters {
        assert!(
            production_sources.contains(source_pattern),
            "miss reason {reason:?} has no production emitter"
        );
    }
}

#[tokio::test]
async fn miss_attribution_keeps_the_most_specific_observation() {
    let (_, reason, _) = capture_miss_reason(Box::pin(async {
        record_miss_reason(miss_reason::CONTEXT_NOT_FOUND);
        record_miss_reason(miss_reason::VERSION_SKEW);
        record_miss_reason(miss_reason::INPUT_FINGERPRINT_MISMATCH);
        record_miss_reason(miss_reason::DESTINATION_WRITE_FAILED);
    }))
    .await;
    assert_eq!(reason, Some(miss_reason::DESTINATION_WRITE_FAILED));
}

#[tokio::test]
async fn compile_scope_captures_the_context_key() {
    let key = crate::depgraph::ContextKey::from_raw([0x2a; 32]);
    let (_, _, captured) = capture_miss_reason(Box::pin(async {
        record_context_key(&key);
    }))
    .await;
    assert_eq!(captured, Some("2a".repeat(32)));
}

// ─── JournalEntry::new miss_reason threading ──────────────────────────────

#[test]
fn journal_entry_new_records_miss_reason() {
    // JournalEntry::new is the canonical builder used by the dispatch
    // site. It must thread miss_reason through to the field.
    let ctx = JournalContext {
        compiler: "rustc".into(),
        args: vec![],
        cwd: "/tmp".into(),
        env: None,
        session_id: None,
    };
    let entry = JournalEntry::new(ctx, "miss", 0, 0, Some(miss_reason::CONTEXT_NOT_FOUND));
    assert_eq!(entry.miss_reason.as_deref(), Some("context_not_found"));
}

#[test]
fn journal_entry_new_hit_omits_miss_reason() {
    let ctx = JournalContext {
        compiler: "rustc".into(),
        args: vec![],
        cwd: "/tmp".into(),
        env: None,
        session_id: None,
    };
    let entry = JournalEntry::new(ctx, "hit", 0, 0, None);
    assert_eq!(entry.miss_reason, None);
    let json = serde_json::to_string(&entry).unwrap();
    assert!(
        !json.contains("\"miss_reason\""),
        "hit entries must omit miss_reason: {json}"
    );
}

// ─── End-to-end JSONL miss-reason wiring ──────────────────────────────────

#[test]
fn journal_jsonl_miss_record_includes_miss_reason_field() {
    // Integration: writing a Response::CompileResult { cached: false } to
    // the journal must produce a JSONL line that includes "miss_reason".
    // This is the field consumers (setup-soldr) read.
    let dir = tempfile::tempdir().unwrap();
    let journal = CompileJournal::new(dir.path().to_path_buf().into());

    let ctx = JournalContext {
        compiler: "/usr/bin/rustc".into(),
        args: vec!["--crate-name".into(), "foo".into()],
        cwd: "/project".into(),
        env: None,
        session_id: None,
    };
    // Use the canonical extractor so the test exercises the same path the
    // real dispatcher does.
    let resp = Response::CompileResult {
        exit_code: 0,
        stdout: Arc::new(vec![]),
        stderr: Arc::new(vec![]),
        cached: false,
    };
    let (outcome, exit_code, miss) = extract_outcome(&resp).unwrap();
    journal.log(
        &JournalEntry::new(ctx, outcome, exit_code, 1_000_000, miss),
        None,
    );

    let path = dir.path().join("compile_journal.jsonl");
    wait_for_lines(&path, 1);
    let content = fs::read_to_string(&path).unwrap();
    let line = content.lines().next().expect("expected one journal line");
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(v["outcome"], "miss");
    assert!(
        v.get("miss_reason").is_some(),
        "miss record must have miss_reason field: {line}"
    );
    // The default value is the documented 'unknown' bucket — concrete
    // attributions are layered on in follow-ups.
    assert_eq!(v["miss_reason"], "unknown");
}

#[test]
fn journal_jsonl_hit_record_omits_miss_reason() {
    let dir = tempfile::tempdir().unwrap();
    let journal = CompileJournal::new(dir.path().to_path_buf().into());

    let ctx = JournalContext {
        compiler: "/usr/bin/rustc".into(),
        args: vec![],
        cwd: "/project".into(),
        env: None,
        session_id: None,
    };
    let resp = Response::CompileResult {
        exit_code: 0,
        stdout: Arc::new(vec![]),
        stderr: Arc::new(vec![]),
        cached: true,
    };
    let (outcome, exit_code, miss) = extract_outcome(&resp).unwrap();
    journal.log(
        &JournalEntry::new(ctx, outcome, exit_code, 1_000_000, miss),
        None,
    );

    let path = dir.path().join("compile_journal.jsonl");
    wait_for_lines(&path, 1);
    let content = fs::read_to_string(&path).unwrap();
    let line = content.lines().next().expect("expected one journal line");
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(v["outcome"], "hit");
    assert!(
        v.get("miss_reason").is_none(),
        "hit record must omit miss_reason: {line}"
    );
}

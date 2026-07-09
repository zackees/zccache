//! Tests for `JournalEntry` serialization (legacy + extended profile fields)
//! and the `SelfProfileSpans` accumulator / `with_profile_fields` builder.

use super::super::{miss_reason, JournalEntry, MissDiff, SelfProfileNs, SelfProfileSpans};
use super::{legacy_entry, make_ctx};

// ─── Legacy JournalEntry serialization ────────────────────────────────────

#[test]
fn test_journal_entry_serialization() {
    let entry = legacy_entry(
        "2026-03-17T10:30:00.123Z",
        "hit",
        "/usr/bin/clang++",
        vec!["-c".to_string(), "foo.cpp".to_string()],
        "/project/build",
        Some(vec![("CC".to_string(), "clang".to_string())]),
        0,
        Some("test-uuid".to_string()),
        1_234_567,
    );
    let json = serde_json::to_string(&entry).unwrap();
    assert!(json.contains("\"outcome\":\"hit\""), "json: {json}");
    assert!(
        json.contains("\"compiler\":\"/usr/bin/clang++\""),
        "json: {json}"
    );
    assert!(json.contains("\"latency_ns\":1234567"), "json: {json}");
    assert!(
        json.contains("\"env\":[[\"CC\",\"clang\"]]"),
        "json: {json}"
    );
    // Verify it's valid JSON
    let _: serde_json::Value = serde_json::from_str(&json).unwrap();
}

#[test]
fn test_journal_entry_env_omitted_when_none() {
    let entry = legacy_entry(
        "2026-03-17T10:30:00.123Z",
        "miss",
        "clang",
        vec![],
        "/tmp",
        None,
        0,
        None,
        0,
    );
    let json = serde_json::to_string(&entry).unwrap();
    assert!(!json.contains("\"env\""), "env should be omitted: {json}");
    assert!(json.contains("\"session_id\":null"), "json: {json}");
}

// ─── Serialization edge cases ─────────────────────────────────────────────

#[test]
fn test_serialization_empty_fields() {
    let entry = legacy_entry("", "miss", "", vec![], "", None, 0, None, 0);
    let json = serde_json::to_string(&entry).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["compiler"], "");
    assert_eq!(v["args"].as_array().unwrap().len(), 0);
    assert_eq!(v["cwd"], "");
}

#[test]
fn test_serialization_special_characters() {
    let entry = legacy_entry(
        "2026-03-17T10:30:00Z",
        "hit",
        r"C:\Program Files\LLVM\bin\clang++.exe",
        vec![
            "-DFOO=\"bar baz\"".to_string(),
            "-I/path/with spaces".to_string(),
            "file\twith\ttabs.cpp".to_string(),
        ],
        "/home/用户/项目",
        Some(vec![("PATH".to_string(), r"C:\a;C:\b".to_string())]),
        0,
        None,
        42,
    );
    let json = serde_json::to_string(&entry).unwrap();
    // Must parse back to identical values
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["compiler"], r"C:\Program Files\LLVM\bin\clang++.exe");
    assert_eq!(v["cwd"], "/home/用户/项目");
    assert_eq!(v["args"][0], "-DFOO=\"bar baz\"");
}

#[test]
fn test_serialization_large_args() {
    let args: Vec<String> = (0..10_000).map(|i| format!("-DVAR_{i}=val")).collect();
    let entry = legacy_entry(
        "2026-03-17T10:30:00Z",
        "miss",
        "clang",
        args,
        "/tmp",
        None,
        0,
        None,
        0,
    );
    let json = serde_json::to_string(&entry).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["args"].as_array().unwrap().len(), 10_000);
}

#[test]
fn test_serialization_u128_max_latency() {
    let entry = legacy_entry(
        "2026-03-17T10:30:00Z",
        "miss",
        "clang",
        vec![],
        "/tmp",
        None,
        0,
        None,
        u128::MAX,
    );
    // Serialization itself must succeed.
    let json = serde_json::to_string(&entry).unwrap();
    assert!(json.contains("latency_ns"));

    // But parsing back through serde_json::Value loses precision:
    // u128::MAX > 2^53, so the JSON number becomes f64, and as_u64() returns None.
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(
        v["latency_ns"].as_u64().is_none(),
        "u128::MAX should not round-trip through serde_json::Value as u64"
    );
    // The value exists but is a lossy float
    assert!(v["latency_ns"].is_number(), "should still be a number");
}

#[test]
fn test_serialization_newline_injection() {
    // Newlines in fields must not corrupt JSONL (one JSON object per line).
    // serde_json should escape them as \n in the JSON string.
    let entry = legacy_entry(
        "2026-03-17T10:30:00Z",
        "miss",
        "clang",
        vec!["-DMSG=\"line1\nline2\"".to_string()],
        "/tmp",
        None,
        0,
        None,
        0,
    );
    let json = serde_json::to_string(&entry).unwrap();
    // The serialized JSON must be a single line (no raw newlines)
    assert_eq!(
        json.lines().count(),
        1,
        "JSON output must be single-line for JSONL: {json}"
    );
    // Round-trip preserves the embedded newline
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(v["args"][0].as_str().unwrap().contains('\n'));
}

#[test]
fn test_serialization_control_chars_and_null_bytes() {
    // Strings with control characters (including NUL) must serialize to valid JSON.
    let entry = legacy_entry(
        "2026-03-17T10:30:00Z",
        "hit",
        "clang",
        vec![
            "has\0null".to_string(),
            "has\x01ctrl".to_string(),
            "has\x7fDEL".to_string(),
        ],
        "/tmp",
        None,
        0,
        None,
        0,
    );
    let json = serde_json::to_string(&entry).unwrap();
    // Must produce valid single-line JSON
    assert_eq!(json.lines().count(), 1);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(v["args"][0].as_str().unwrap().contains('\0'));
}

#[test]
fn test_serialization_exit_code_boundary_values() {
    // Ensure extreme exit codes serialize and round-trip correctly
    for exit_code in [i32::MIN, -1, 0, 1, 127, 255, i32::MAX] {
        let entry = legacy_entry(
            "2026-03-17T10:30:00Z",
            "error",
            "clang",
            vec![],
            "/tmp",
            None,
            exit_code,
            None,
            0,
        );
        let json = serde_json::to_string(&entry).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            v["exit_code"].as_i64().unwrap(),
            exit_code as i64,
            "exit_code {exit_code} round-trip failed"
        );
    }
}

#[test]
fn test_serialization_latency_precision_boundary() {
    // serde_json::Value stores numbers as i64/u64/f64 internally.
    // Values up to u64::MAX round-trip exactly (stored as u64).
    // Values above u64::MAX (u128 range) fall back to f64 and lose precision.
    // This is better than most JSON parsers (Python/JS lose precision at 2^53).

    // u64::MAX round-trips exactly through serde_json::Value
    let entry_u64max = legacy_entry(
        "2026-03-17T10:30:00Z",
        "miss",
        "clang",
        vec![],
        "/tmp",
        None,
        0,
        None,
        u64::MAX as u128,
    );
    let json = serde_json::to_string(&entry_u64max).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        v["latency_ns"].as_u64(),
        Some(u64::MAX),
        "u64::MAX should round-trip exactly through serde_json::Value"
    );

    // u64::MAX + 1 does NOT round-trip (falls to f64)
    let entry_above = legacy_entry(
        "2026-03-17T10:30:00Z",
        "miss",
        "clang",
        vec![],
        "/tmp",
        None,
        0,
        None,
        u64::MAX as u128 + 1,
    );
    let json2 = serde_json::to_string(&entry_above).unwrap();
    let v2: serde_json::Value = serde_json::from_str(&json2).unwrap();
    assert!(
        v2["latency_ns"].as_u64().is_none(),
        "u64::MAX+1 should NOT parse as u64 through serde_json::Value"
    );
}

// ─── Profile-mode schema extension (issue #256) ───────────────────────────

#[test]
fn serializes_extended_fields_when_present() {
    let entry = JournalEntry {
        ts: "2026-03-17T10:30:00.123Z".to_string(),
        outcome: "miss",
        compiler: "/usr/bin/rustc".to_string(),
        args: vec!["--crate-name".into(), "soldr_cli".into()],
        cwd: "/project".to_string(),
        env: None,
        exit_code: 0,
        session_id: Some("sess-1".to_string()),
        latency_ns: 1_234_567,
        crate_name: Some("soldr_cli".to_string()),
        crate_type: Some("bin".to_string()),
        output_ext: Some("exe".to_string()),
        miss_reason: Some("inputs".to_string()),
        miss_diff: Some(MissDiff {
            changed_files: vec!["src/main.rs".to_string(), "build.rs".to_string()],
            changed_flags: vec!["-C".to_string(), "debuginfo=2".to_string()],
            changed_deps: vec!["serde@1.0.213".to_string()],
        }),
        self_profile_ns: Some(SelfProfileNs {
            hash_inputs: 12_400_000,
            lookup: 410_000,
            decompress: 14_100_000,
            store: 203_000_000,
        }),
    };

    let json = serde_json::to_string(&entry).unwrap();

    // Legacy fields still serialize.
    assert!(json.contains("\"outcome\":\"miss\""), "json: {json}");
    assert!(
        json.contains("\"compiler\":\"/usr/bin/rustc\""),
        "json: {json}"
    );
    assert!(json.contains("\"latency_ns\":1234567"), "json: {json}");

    // Each new key appears exactly once.
    assert!(
        json.contains("\"crate_name\":\"soldr_cli\""),
        "json: {json}"
    );
    assert!(json.contains("\"crate_type\":\"bin\""), "json: {json}");
    assert!(json.contains("\"output_ext\":\"exe\""), "json: {json}");
    assert!(json.contains("\"miss_reason\":\"inputs\""), "json: {json}");
    assert!(json.contains("\"miss_diff\""), "json: {json}");
    assert!(json.contains("\"self_profile_ns\""), "json: {json}");

    // Round-trip through serde_json::Value to confirm structure.
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["crate_name"], "soldr_cli");
    assert_eq!(v["crate_type"], "bin");
    assert_eq!(v["output_ext"], "exe");
    assert_eq!(v["miss_reason"], "inputs");
    assert_eq!(v["miss_diff"]["changed_files"][0], "src/main.rs");
    assert_eq!(v["miss_diff"]["changed_files"][1], "build.rs");
    assert_eq!(v["miss_diff"]["changed_flags"][0], "-C");
    assert_eq!(v["miss_diff"]["changed_flags"][1], "debuginfo=2");
    assert_eq!(v["miss_diff"]["changed_deps"][0], "serde@1.0.213");
    assert_eq!(v["self_profile_ns"]["hash_inputs"], 12_400_000_u64);
    assert_eq!(v["self_profile_ns"]["lookup"], 410_000_u64);
    assert_eq!(v["self_profile_ns"]["decompress"], 14_100_000_u64);
    assert_eq!(v["self_profile_ns"]["store"], 203_000_000_u64);
}

#[test]
fn omits_extended_fields_when_none() {
    // A legacy-shape JournalEntry (constructed via JournalEntry::new) must
    // not emit any of the new keys. This protects the default-off invariant.
    let ctx = super::super::JournalContext {
        compiler: "/usr/bin/clang++".to_string(),
        args: vec!["-c".to_string(), "test.cpp".to_string()],
        cwd: "/project".to_string(),
        env: None,
        session_id: Some("s".to_string()),
    };
    let entry = JournalEntry::new(ctx, "hit", 0, 1000, None);
    let json = serde_json::to_string(&entry).unwrap();

    for forbidden in [
        "\"crate_name\"",
        "\"crate_type\"",
        "\"output_ext\"",
        "\"miss_reason\"",
        "\"miss_diff\"",
        "\"self_profile_ns\"",
    ] {
        assert!(
            !json.contains(forbidden),
            "legacy journal must omit {forbidden}: {json}"
        );
    }
}

// ─── SelfProfileSpans / with_profile_fields (issue #256) ──────────────────

#[test]
fn with_profile_fields_populates_crate_name_type_and_ext() {
    let ctx = make_ctx(vec![
        "--crate-name",
        "soldr_cli",
        "--crate-type",
        "bin",
        "src/main.rs",
    ]);
    let entry = JournalEntry::new(ctx, "hit", 0, 1_234, None).with_profile_fields(None);
    assert_eq!(entry.crate_name.as_deref(), Some("soldr_cli"));
    assert_eq!(entry.crate_type.as_deref(), Some("bin"));
    assert_eq!(entry.output_ext.as_deref(), Some("exe"));
    assert!(entry.self_profile_ns.is_none());
}

#[test]
fn with_profile_fields_threads_self_profile_spans() {
    let ctx = make_ctx(vec!["--crate-name", "x", "--crate-type", "lib"]);
    let mut spans = SelfProfileSpans::default();
    spans.add_hash_inputs_ns(11);
    spans.add_lookup_ns(22);
    spans.add_decompress_ns(33);
    spans.add_store_ns(44);
    let entry = JournalEntry::new(ctx, "hit", 0, 0, None).with_profile_fields(Some(spans));
    let sp = entry.self_profile_ns.unwrap();
    assert_eq!(sp.hash_inputs, 11);
    assert_eq!(sp.lookup, 22);
    assert_eq!(sp.decompress, 33);
    assert_eq!(sp.store, 44);
}

#[test]
fn with_profile_fields_emits_empty_miss_diff_on_miss() {
    // Issue #340 acceptance criterion: a `--profile` miss must always include
    // a `miss_diff` object, even when no prior context is available to diff
    // against (first-seen miss). The empty-arrays form is the distinguishing
    // signal between "diff was computed but nothing changed" and "diff was
    // not computed because --profile is off" (the latter omits the field).
    let ctx = make_ctx(vec!["--crate-name", "x", "--crate-type", "lib"]);
    let entry = JournalEntry::new(ctx, "miss", 0, 1_234, Some(miss_reason::UNKNOWN))
        .with_profile_fields(None);
    let diff = entry.miss_diff.as_ref().expect("miss_diff must be Some");
    assert!(diff.changed_files.is_empty());
    assert!(diff.changed_flags.is_empty());
    assert!(diff.changed_deps.is_empty());
}

#[test]
fn with_profile_fields_emits_empty_miss_diff_on_link_miss() {
    let ctx = make_ctx(vec!["--crate-name", "x", "--crate-type", "lib"]);
    let entry = JournalEntry::new(ctx, "link_miss", 0, 1_234, Some(miss_reason::UNKNOWN))
        .with_profile_fields(None);
    assert!(entry.miss_diff.is_some());
}

#[test]
fn with_profile_fields_omits_miss_diff_on_hit() {
    // Hits never carry miss_diff regardless of --profile state.
    let ctx = make_ctx(vec!["--crate-name", "x", "--crate-type", "lib"]);
    let entry = JournalEntry::new(ctx, "hit", 0, 1_234, None).with_profile_fields(None);
    assert!(entry.miss_diff.is_none());
}

#[test]
fn legacy_entry_without_with_profile_fields_omits_all_new_fields() {
    // Issue #256 acceptance criterion: when --profile is OFF the
    // journal record must serialize without crate_name, crate_type,
    // output_ext, self_profile_ns, or miss_diff.
    let ctx = make_ctx(vec!["--crate-name", "x", "--crate-type", "lib"]);
    let entry = JournalEntry::new(ctx, "hit", 0, 5, None);
    let json = serde_json::to_string(&entry).unwrap();
    for absent in [
        "\"crate_name\"",
        "\"crate_type\"",
        "\"output_ext\"",
        "\"miss_diff\"",
        "\"self_profile_ns\"",
    ] {
        assert!(
            !json.contains(absent),
            "non-profile entry must omit {absent}, got: {json}"
        );
    }
}

#[test]
fn profile_entry_roundtrips_through_serde() {
    // Issue #256 acceptance criterion: a journal line with every
    // extended field set must serialize and deserialize losslessly.
    let ctx = make_ctx(vec!["--crate-name", "y", "--crate-type", "proc-macro"]);
    let mut spans = SelfProfileSpans::default();
    spans.add_hash_inputs_ns(100);
    spans.add_lookup_ns(200);
    let mut entry = JournalEntry::new(ctx, "miss", 0, 999, Some(miss_reason::UNKNOWN))
        .with_profile_fields(Some(spans));
    entry.miss_diff = Some(MissDiff {
        changed_files: vec!["src/lib.rs".to_string()],
        changed_flags: vec!["-C".into(), "debuginfo=2".into()],
        changed_deps: vec!["serde@1.0.213".into()],
    });
    let json = serde_json::to_string(&entry).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["crate_name"], "y");
    assert_eq!(v["crate_type"], "proc-macro");
    assert_eq!(v["output_ext"], "so");
    assert_eq!(v["miss_reason"], "unknown");
    assert_eq!(v["miss_diff"]["changed_files"][0], "src/lib.rs");
    assert_eq!(v["miss_diff"]["changed_flags"][1], "debuginfo=2");
    assert_eq!(v["miss_diff"]["changed_deps"][0], "serde@1.0.213");
    assert_eq!(v["self_profile_ns"]["hash_inputs"], 100);
    assert_eq!(v["self_profile_ns"]["lookup"], 200);
}

#[test]
fn self_profile_spans_saturate_on_overflow() {
    // Issue #256: span accumulator must not panic when a malformed
    // measurement returns u128::MAX. Saturating arithmetic preserves
    // the journal contract: a span never observes a smaller value.
    let mut spans = SelfProfileSpans::default();
    spans.add_hash_inputs_ns(u128::MAX);
    spans.add_hash_inputs_ns(5);
    assert_eq!(spans.hash_inputs_ns, u128::MAX);
}

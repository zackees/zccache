//! Miss-reason attribution + self-profile-span unit tests for
//! `server/connection/`. Moved out of `connection.rs`'s
//! `self_profile_tests` module during the #1154 phase-0 split
//! (`crates/CLAUDE.md` § File-size discipline).

use super::super::connection::{
    append_unknown_miss_warning, compile_miss_reason, derive_approx_spans, redacted_args_preview,
};
use super::super::*;

fn test_journal_ctx(compiler: &str, args: &[&str]) -> JournalContext {
    JournalContext {
        compiler: compiler.to_string(),
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
        cwd: ".".to_string(),
        env: None,
        session_id: None,
    }
}

fn compile_miss_reason_with_tmp_root(
    ctx: &JournalContext,
    outcome: &str,
    default_reason: Option<&'static str>,
    latency_ns: u128,
) -> Option<&'static str> {
    let cache_root = tempfile::tempdir().unwrap();
    compile_miss_reason(ctx, outcome, default_reason, latency_ns, cache_root.path())
}

#[test]
fn parse_time_non_cacheable_miss_is_attributed() {
    let ctx = test_journal_ctx("rustc", &["--version"]);
    assert_eq!(
        compile_miss_reason_with_tmp_root(&ctx, "miss", Some(miss_reason::UNKNOWN), 0),
        Some(miss_reason::UNCACHEABLE_INPUT)
    );
}

#[test]
fn cacheable_miss_keeps_default_reason() {
    let cache = tempfile::tempdir().unwrap();
    let ctx = test_journal_ctx(
        "rustc",
        &[
            "--crate-name",
            "demo",
            "src/lib.rs",
            "--extern",
            "api_token=do-not-log-this",
        ],
    );
    assert_eq!(
        compile_miss_reason(&ctx, "miss", Some(miss_reason::UNKNOWN), 42, cache.path(),),
        Some(miss_reason::UNKNOWN)
    );
    let lifecycle = std::fs::read_to_string(
        cache
            .path()
            .join("logs")
            .join(crate::core::lifecycle::LIVE_LOG_FILENAME),
    )
    .unwrap();
    let event: serde_json::Value = lifecycle
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .find(|event: &serde_json::Value| {
            event["event"] == crate::core::lifecycle::EVENT_MISS_REASON_UNKNOWN
        })
        .expect("unknown event");
    assert_eq!(event["branch"], "connection::compile_miss_reason");
    assert_eq!(event["latency_ns"], 42);
    assert_eq!(event["artifact_key"], serde_json::Value::Null);
    let serialized = event.to_string();
    assert!(serialized.contains("api_token=<redacted>"));
    assert!(!serialized.contains("do-not-log-this"));

    let mut response = Response::CompileResult {
        exit_code: 0,
        stdout: Arc::new(Vec::new()),
        stderr: Arc::new(Vec::new()),
        cached: false,
    };
    append_unknown_miss_warning(&mut response, &ctx, 42);
    let Response::CompileResult { stderr, .. } = response else {
        unreachable!()
    };
    let warning = String::from_utf8(stderr.as_ref().clone()).unwrap();
    assert!(warning.contains("artifact_key=<unavailable>"));
    assert!(warning.contains("branch=connection::compile_miss_reason"));

    let report = crate::audit::audit_cache_root(
        cache.path(),
        crate::audit::LogAuditContext::Integration,
        &crate::audit::AuditOptions::default().allow_for_test(
            "connection::cacheable_miss_keeps_default_reason",
            [crate::audit::RuleId("no-unknown-miss-reason")],
        ),
    )
    .unwrap();
    assert!(report.passed(), "{}", report.format_human());
    assert_eq!(
        report.test_allow_name.as_deref(),
        Some("connection::cacheable_miss_keeps_default_reason")
    );
}

#[test]
fn classified_miss_does_not_request_unknown_warning() {
    let ctx = test_journal_ctx("rustc", &["--crate-name", "demo", "src/lib.rs"]);
    let reason =
        compile_miss_reason_with_tmp_root(&ctx, "miss", Some(miss_reason::CONTEXT_NOT_FOUND), 1);
    let mut response = Response::CompileResult {
        exit_code: 0,
        stdout: Arc::new(Vec::new()),
        stderr: Arc::new(Vec::new()),
        cached: false,
    };
    if reason == Some(miss_reason::UNKNOWN) {
        append_unknown_miss_warning(&mut response, &ctx, 1);
    }
    let Response::CompileResult { stderr, .. } = response else {
        unreachable!()
    };
    assert!(stderr.is_empty());
}

// Root cause: `record_miss_reason` writes into a single task-local slot for
// the whole request (`capture_miss_reason`'s scope), and the depgraph-verdict
// classifier in `pipeline/mod.rs` calls it for every verdict — including
// `CacheVerdict::Hit` — purely for bookkeeping, before the hit path even
// tries to materialize the artifact. When that materialization succeeds, the
// request resolves as a genuine hit, but the task-local slot is left holding
// `no_artifact_for_key` from the earlier verdict-classification call. Without
// this guard that stale value leaks straight into the journal row's
// `miss_reason` field even though `outcome == "hit"` — a self-contradictory
// row ("cached: true" with a miss reason) that confused CI triage.
#[test]
fn hit_outcome_never_carries_a_leaked_miss_reason() {
    let ctx = test_journal_ctx("cc", &["-c", "main.c", "-o", "main.o"]);
    // `default_reason` simulates `attributed_miss_reason.or(miss_reason)` at
    // the call site in `connection/mod.rs`: the depgraph-verdict classifier
    // stamped `no_artifact_for_key` on the task-local slot before the hit
    // path found the artifact and returned successfully.
    for leaked in [
        miss_reason::NO_ARTIFACT_FOR_KEY,
        miss_reason::CONTEXT_NOT_FOUND,
        miss_reason::INPUT_FINGERPRINT_MISMATCH,
    ] {
        assert_eq!(
            compile_miss_reason_with_tmp_root(&ctx, "hit", Some(leaked), 0),
            None,
            "a hit outcome must never surface a leaked miss_reason ({leaked})"
        );
    }
    // Same invariant for the other non-miss outcomes.
    for outcome in ["link_hit", "error", "cached_error"] {
        assert_eq!(
            compile_miss_reason_with_tmp_root(
                &ctx,
                outcome,
                Some(miss_reason::NO_ARTIFACT_FOR_KEY),
                0
            ),
            None,
            "outcome {outcome} must never surface a leaked miss_reason"
        );
    }
}

#[test]
fn unknown_preview_redacts_separate_sensitive_values() {
    let args = vec![
        "--authorization".to_string(),
        "Bearer should-not-appear".to_string(),
        "src/lib.rs".to_string(),
    ];
    let preview = redacted_args_preview(&args);
    assert_eq!(preview[0], "--authorization");
    assert_eq!(preview[1], "<redacted-sensitive-value>");
    assert_eq!(preview[2], "src/lib.rs");
    assert!(!preview.join(" ").contains("should-not-appear"));
}

// Issue #951: fbuild-style invocations pass the whole cacheable
// argv through `@file.rsp`. Attribution must expand the response
// file before parsing — parsing the raw `@arg` mislabels every
// such miss as `uncacheable_input`.
#[test]
fn rsp_cacheable_miss_keeps_default_reason() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("file1.cpp");
    std::fs::write(&src, "int f() { return 1; }\n").unwrap();
    let rsp = dir.path().join("compile_1.rsp");
    std::fs::write(&rsp, format!("-c\n{}\n-o\nfile1.o\n-O2\n", src.display())).unwrap();
    let arg = format!("@{}", rsp.display());
    let mut ctx = test_journal_ctx("/usr/bin/g++", &[arg.as_str()]);
    ctx.cwd = dir.path().to_string_lossy().into_owned();
    assert_eq!(
        compile_miss_reason_with_tmp_root(&ctx, "miss", Some(miss_reason::UNKNOWN), 0),
        Some(miss_reason::UNKNOWN),
        "a cacheable compile behind @rsp must not be stamped uncacheable_input"
    );
}

// Issue #951: a genuinely uncacheable invocation stays attributed
// even when it arrives through a response file.
#[test]
fn rsp_preprocess_only_miss_is_attributed_uncacheable() {
    let dir = tempfile::tempdir().unwrap();
    let rsp = dir.path().join("preprocess.rsp");
    std::fs::write(&rsp, "-E\nfile1.cpp\n").unwrap();
    let arg = format!("@{}", rsp.display());
    let mut ctx = test_journal_ctx("/usr/bin/g++", &[arg.as_str()]);
    ctx.cwd = dir.path().to_string_lossy().into_owned();
    assert_eq!(
        compile_miss_reason_with_tmp_root(&ctx, "miss", Some(miss_reason::UNKNOWN), 0),
        Some(miss_reason::UNCACHEABLE_INPUT)
    );
}

// Issue #951: fbuild deletes the rsp right after the compile; if it
// is already gone at journal time, keep the honest `unknown`
// default rather than guessing uncacheable.
#[test]
fn rsp_missing_at_journal_time_keeps_default_reason() {
    let ctx = test_journal_ctx("/usr/bin/g++", &["@/nonexistent/gone.rsp"]);
    assert_eq!(
        compile_miss_reason_with_tmp_root(&ctx, "miss", Some(miss_reason::UNKNOWN), 0),
        Some(miss_reason::UNKNOWN)
    );
}

#[test]
fn hit_split_has_non_zero_hash_lookup_decompress() {
    let s = derive_approx_spans("hit", 999).unwrap();
    assert_ne!(s.hash_inputs_ns, 0);
    assert_ne!(s.lookup_ns, 0);
    assert_ne!(s.decompress_ns, 0);
    assert_eq!(s.store_ns, 0);
    assert_eq!(s.hash_inputs_ns + s.lookup_ns + s.decompress_ns, 999);
}

#[test]
fn miss_split_has_non_zero_hash_lookup_store() {
    let s = derive_approx_spans("miss", 999).unwrap();
    assert_ne!(s.hash_inputs_ns, 0);
    assert_ne!(s.lookup_ns, 0);
    assert_ne!(s.store_ns, 0);
    assert_eq!(s.decompress_ns, 0);
    assert_eq!(s.hash_inputs_ns + s.lookup_ns + s.store_ns, 999);
}

#[test]
fn link_outcomes_partition_too() {
    assert!(derive_approx_spans("link_hit", 100).is_some());
    assert!(derive_approx_spans("link_miss", 100).is_some());
}

#[test]
fn error_outcome_returns_none() {
    assert!(derive_approx_spans("error", 100).is_none());
}

//! Fingerprint request/response bincode roundtrip tests:
//! `FingerprintCheck` / `FingerprintMarkSuccess` / `FingerprintMarkFailure` /
//! `FingerprintInvalidate` plus `FingerprintCheckResult` and `FingerprintAck`.

use super::*;

#[test]
fn fingerprint_check_roundtrip() {
    roundtrip(&Request::FingerprintCheck {
        cache_file: "/tmp/lint.json".into(),
        cache_type: "two-layer".into(),
        root: "/home/user/project/src".into(),
        extensions: vec!["rs".into(), "toml".into()],
        include_globs: vec![],
        exclude: vec![".git".into(), "target".into()],
    });
    roundtrip(&Request::FingerprintCheck {
        cache_file: "cache.json".into(),
        cache_type: "hash".into(),
        root: ".".into(),
        extensions: vec![],
        include_globs: vec!["**/*.cpp".into(), "**/*.h".into()],
        exclude: vec![],
    });
}

#[test]
fn fingerprint_mark_success_roundtrip() {
    roundtrip(&Request::FingerprintMarkSuccess {
        cache_file: "/tmp/lint.json".into(),
    });
}

#[test]
fn fingerprint_mark_failure_roundtrip() {
    roundtrip(&Request::FingerprintMarkFailure {
        cache_file: "/tmp/lint.json".into(),
    });
}

#[test]
fn fingerprint_invalidate_roundtrip() {
    roundtrip(&Request::FingerprintInvalidate {
        cache_file: "/tmp/lint.json".into(),
    });
}

#[test]
fn fingerprint_check_result_roundtrip() {
    roundtrip(&Response::FingerprintCheckResult {
        decision: "skip".into(),
        reason: None,
        changed_files: vec![],
    });
    roundtrip(&Response::FingerprintCheckResult {
        decision: "run".into(),
        reason: Some("content changed".into()),
        changed_files: vec!["src/main.rs".into(), "src/lib.rs".into()],
    });
    roundtrip(&Response::FingerprintCheckResult {
        decision: "run".into(),
        reason: Some("no cache file".into()),
        changed_files: vec![],
    });
}

#[test]
fn fingerprint_ack_roundtrip() {
    roundtrip(&Response::FingerprintAck);
}

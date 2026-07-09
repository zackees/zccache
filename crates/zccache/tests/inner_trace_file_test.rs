//! Wire-shape regression test for the `ZCCACHE_INNER_TRACE` sub-phase trace
//! (issue #940). Lives in its own integration-test binary so the trace
//! module's one-shot `OnceLock` env read is guaranteed fresh — a shared unit
//! test could lose the race if another test triggered the writer first.
//!
//! This exercises the exact `compile_trace::record` writer the deep pipeline
//! seams call through `inner_trace::record_ns`; it asserts the JSONL line shape
//! that soldr's `bench/parse_compile_trace.py` parses.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Read;

#[test]
fn inner_trace_writes_jsonl_line_per_subphase() {
    let dir = tempfile::tempdir().expect("temp dir");
    let trace_path = dir.path().join("inner_trace.jsonl");

    // Must be set before the first `record` call so the module's OnceLock
    // picks it up. This is the only test in this binary.
    std::env::set_var("ZCCACHE_INNER_TRACE", &trace_path);

    // Emit one record per sub-phase #940 enumerates, mirroring what the
    // pipeline seams forward.
    for phase in [
        "input_hash",
        "cache_lookup",
        "cache_load",
        "rustc_spawn",
        "rustc_wait",
        "output_read",
        "cache_store",
    ] {
        zccache::compile_trace::record(phase, 1_234, "z0000002a");
    }

    let mut contents = String::new();
    std::fs::File::open(&trace_path)
        .expect("trace file was created")
        .read_to_string(&mut contents)
        .expect("trace file is readable");

    let lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 7, "one JSONL line per sub-phase record");

    // First line carries the exact field order/shape soldr's parser depends on.
    let first = lines[0];
    assert!(
        first.starts_with(r#"{"ts_ns":"#),
        "line starts with ts_ns: {first}"
    );
    assert!(
        first.contains(r#""phase":"input_hash""#),
        "phase field present: {first}"
    );
    assert!(
        first.contains(r#""micros":1234"#),
        "micros field present: {first}"
    );
    assert!(
        first.contains(r#""compile_id":"z0000002a""#),
        "compile_id field present: {first}"
    );

    // Every enumerated sub-phase name made it to disk.
    for phase in [
        "input_hash",
        "cache_lookup",
        "cache_load",
        "rustc_spawn",
        "rustc_wait",
        "output_read",
        "cache_store",
    ] {
        assert!(
            contents.contains(&format!(r#""phase":"{phase}""#)),
            "sub-phase {phase} recorded"
        );
    }
}

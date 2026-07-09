//! `GenericToolExec` (issue #272) roundtrip tests plus
//! `ExecOutputStreams` / `ExecCachePolicy` default checks.

use super::*;

#[test]
fn generic_tool_exec_roundtrip() {
    let req = Request::GenericToolExec {
        tool: "/usr/local/bin/fastled-lint".into(),
        args: vec!["src/foo.cpp".into(), "--json".into()],
        cwd: "/home/user/project".into(),
        env: vec![
            ("PATH".into(), "/usr/bin".into()),
            ("LINT_VERSION".into(), "1.2.3".into()),
        ],
        input_files: vec!["src/foo.cpp".into(), "ci/lint_cpp_rs/rules.json".into()],
        input_extra: Arc::new(b"namespace-tag".to_vec()),
        output_streams: ExecOutputStreams::default(),
        output_files: vec!["report.json".into()],
        tool_hash: Some([0x42; 32]),
        cache_policy: ExecCachePolicy::Normal,
        cwd_in_key: true,
        include_scan_files: vec!["src/foo.cpp".into()],
        include_dirs: vec!["src".into(), "include".into()],
        system_include_dirs: vec!["/usr/include".into()],
        iquote_dirs: vec!["thirdparty/q".into()],
        depfile: Some("target/lint/foo.d".into()),
        non_deterministic: false,
        key_args_filter: vec!["^--verbose$".into(), "^--no-color$".into()],
    };
    roundtrip(&req);

    // Bypass + None tool_hash + empty inputs path.
    let req_bypass = Request::GenericToolExec {
        tool: "/bin/true".into(),
        args: vec![],
        cwd: ".".into(),
        env: vec![],
        input_files: vec![],
        input_extra: Arc::new(Vec::new()),
        output_streams: ExecOutputStreams {
            stdout: true,
            stderr: false,
        },
        output_files: vec![],
        tool_hash: None,
        cache_policy: ExecCachePolicy::Bypass,
        cwd_in_key: false,
        include_scan_files: vec![],
        include_dirs: vec![],
        system_include_dirs: vec![],
        iquote_dirs: vec![],
        depfile: None,
        non_deterministic: true,
        key_args_filter: vec![],
    };
    roundtrip(&req_bypass);

    let resp = Response::GenericToolExecResult {
        exit_code: 0,
        stdout: Arc::new(b"linted ok\n".to_vec()),
        stderr: Arc::new(Vec::new()),
        output_files: vec![ArtifactOutput {
            name: "report.json".into(),
            payload: ArtifactPayload::Bytes(Arc::new(b"{}".to_vec())),
        }],
        cached: true,
        cache_key_hex: "deadbeef".repeat(8),
    };
    roundtrip(&resp);
}

#[test]
fn exec_output_streams_default_captures_both() {
    let s = ExecOutputStreams::default();
    assert!(s.stdout);
    assert!(s.stderr);
}

#[test]
fn exec_cache_policy_default_is_normal() {
    assert_eq!(ExecCachePolicy::default(), ExecCachePolicy::Normal);
}

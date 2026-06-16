//! Ephemeral wrapper-mode request roundtrip tests:
//! `CompileEphemeral`, `LinkEphemeral`, `LinkResult`, plus legacy
//! `Request` / `Response` variants that must keep round-tripping.

use super::*;

#[test]
fn compile_ephemeral_roundtrip() {
    roundtrip(&Request::CompileEphemeral {
        client_pid: 9876,
        working_dir: "/home/user/project".into(),
        compiler: "/usr/bin/clang++".into(),
        args: vec!["-c".into(), "main.cpp".into(), "-o".into(), "main.o".into()],
        cwd: "/home/user/project/build".into(),
        env: Some(vec![("PATH".into(), "/usr/bin".into())]),
        stdin: Vec::new(),
    });
    // Non-empty stdin payload must round-trip byte-for-byte — including
    // embedded NULs and binary bytes — so `rustc -` style invocations
    // through the wrapper see the same input the parent sent us.
    roundtrip(&Request::CompileEphemeral {
        client_pid: 1,
        working_dir: ".".into(),
        compiler: "gcc".into(),
        args: vec![],
        cwd: ".".into(),
        env: None,
        stdin: b"hello\x00world\nbinary\xff\xfe".to_vec(),
    });
}

#[test]
fn link_ephemeral_roundtrip() {
    roundtrip(&Request::LinkEphemeral {
        client_pid: 5555,
        tool: "/usr/bin/ar".into(),
        args: vec!["rcs".into(), "libfoo.a".into(), "a.o".into(), "b.o".into()],
        cwd: "/home/user/project/build".into(),
        env: Some(vec![("PATH".into(), "/usr/bin".into())]),
    });
    roundtrip(&Request::LinkEphemeral {
        client_pid: 1,
        tool: "lib.exe".into(),
        args: vec!["/OUT:foo.lib".into(), "a.obj".into()],
        cwd: ".".into(),
        env: None,
    });
}

#[test]
fn link_result_roundtrip() {
    roundtrip(&Response::LinkResult {
        exit_code: 0,
        stdout: Arc::new(vec![]),
        stderr: Arc::new(vec![]),
        cached: true,
        warning: None,
    });
    roundtrip(&Response::LinkResult {
        exit_code: 0,
        stdout: Arc::new(vec![]),
        stderr: Arc::new(b"some warning".to_vec()),
        cached: false,
        warning: Some("non-deterministic: missing D flag".into()),
    });
}

#[test]
fn existing_request_variants_still_work() {
    roundtrip(&Request::Ping);
    roundtrip(&Request::Shutdown);
    roundtrip(&Request::Status);
    roundtrip(&Request::SessionEnd {
        session_id: "550e8400-e29b-41d4-a716-446655440000".into(),
    });
    roundtrip(&Request::Compile {
        session_id: "550e8400-e29b-41d4-a716-446655440000".into(),
        args: vec!["-c".into(), "foo.c".into()],
        cwd: "/tmp".into(),
        compiler: "/usr/bin/gcc".into(),
        env: None,
        stdin: Vec::new(),
    });
}

#[test]
fn existing_response_variants_still_work() {
    roundtrip(&Response::Pong);
    roundtrip(&Response::ShuttingDown);
    roundtrip(&Response::CompileResult {
        exit_code: 0,
        stdout: Arc::new(vec![]),
        stderr: Arc::new(vec![]),
        cached: true,
    });
    roundtrip(&Response::Error {
        message: "test".into(),
    });
}

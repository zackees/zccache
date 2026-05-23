//! Integration tests for `zccache kv` subcommands.
//!
//! Each test uses an isolated `ZCCACHE_CACHE_DIR` so it never touches the
//! user's real cache directory and never collides with other tests.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use zccache::core::NormalizedPath;

fn zccache_bin() -> NormalizedPath {
    let mut path = std::env::current_exe()
        .expect("current_exe")
        .parent()
        .expect("parent of test binary")
        .parent()
        .expect("target dir")
        .to_path_buf();

    if cfg!(windows) {
        path.push("zccache.exe");
    } else {
        path.push("zccache");
    }

    assert!(
        path.exists(),
        "zccache binary not found at {path:?}. Run `cargo build` first."
    );
    NormalizedPath::new(path)
}

fn run_kv(cache_dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> std::process::Output {
    let bin = zccache_bin();
    let mut cmd = Command::new(bin.as_path());
    cmd.env("ZCCACHE_CACHE_DIR", cache_dir);
    cmd.arg("kv");
    for a in args {
        cmd.arg(a);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    let mut child = cmd.spawn().expect("spawn zccache kv");
    if let Some(bytes) = stdin {
        let mut h = child.stdin.take().expect("stdin pipe");
        h.write_all(bytes).expect("write stdin");
        drop(h);
    }
    child.wait_with_output().expect("wait zccache kv")
}

fn hex_key(seed: &[u8]) -> String {
    let h = blake3::hash(seed);
    let mut out = String::with_capacity(64);
    for b in h.as_bytes() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[test]
#[ignore] // Integration: needs the binary built. Run with `test --integration`.
fn kv_put_then_get_round_trips_binary() {
    let cache = tempfile::tempdir().unwrap();
    let key = hex_key(b"round-trip");
    let payload = (0u8..=255u8).cycle().take(8192).collect::<Vec<_>>();

    let value_path = cache.path().join("payload.bin");
    std::fs::write(&value_path, &payload).unwrap();

    let put = run_kv(
        cache.path(),
        &[
            "put",
            "test",
            &key,
            "--value-from",
            value_path.to_str().unwrap(),
        ],
        None,
    );
    assert!(
        put.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&put.stderr)
    );

    let get = run_kv(cache.path(), &["get", "test", &key], None);
    assert!(
        get.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&get.stderr)
    );
    assert_eq!(get.stdout, payload);
}

#[test]
#[ignore]
fn kv_ls_lists_keys() {
    let cache = tempfile::tempdir().unwrap();
    let keys: Vec<String> = (0u32..3).map(|i| hex_key(&i.to_le_bytes())).collect();
    for (i, k) in keys.iter().enumerate() {
        let payload_path = cache.path().join(format!("v{i}.bin"));
        std::fs::write(&payload_path, format!("v{i}")).unwrap();
        let out = run_kv(
            cache.path(),
            &[
                "put",
                "test",
                k,
                "--value-from",
                payload_path.to_str().unwrap(),
            ],
            None,
        );
        assert!(out.status.success());
    }

    let ls = run_kv(cache.path(), &["ls", "test"], None);
    assert!(ls.status.success());
    let stdout = String::from_utf8(ls.stdout).unwrap();
    for k in &keys {
        assert!(stdout.contains(k), "ls did not list {k}: {stdout}");
    }
}

#[test]
#[ignore]
fn kv_rm_then_get_exits_two() {
    let cache = tempfile::tempdir().unwrap();
    let key = hex_key(b"rm");
    let payload_path = cache.path().join("v.bin");
    std::fs::write(&payload_path, b"hello").unwrap();

    run_kv(
        cache.path(),
        &[
            "put",
            "test",
            &key,
            "--value-from",
            payload_path.to_str().unwrap(),
        ],
        None,
    );
    let rm = run_kv(cache.path(), &["rm", "test", &key], None);
    assert!(rm.status.success());

    let get = run_kv(cache.path(), &["get", "test", &key], None);
    assert_eq!(get.status.code(), Some(2));
    assert!(get.stdout.is_empty());
}

#[test]
#[ignore]
fn kv_clear_namespace_empties_ls() {
    let cache = tempfile::tempdir().unwrap();
    let key = hex_key(b"x");
    let payload_path = cache.path().join("v.bin");
    std::fs::write(&payload_path, b"hello").unwrap();

    run_kv(
        cache.path(),
        &[
            "put",
            "test",
            &key,
            "--value-from",
            payload_path.to_str().unwrap(),
        ],
        None,
    );
    let clear = run_kv(cache.path(), &["clear", "test"], None);
    assert!(clear.status.success());
    let ls = run_kv(cache.path(), &["ls", "test"], None);
    assert!(ls.status.success());
    assert!(ls.stdout.is_empty(), "ls output: {:?}", ls.stdout);
}

#[test]
#[ignore]
fn kv_stats_reports_nonzero_total_after_put() {
    let cache = tempfile::tempdir().unwrap();
    let key = hex_key(b"s");
    let payload_path = cache.path().join("v.bin");
    std::fs::write(&payload_path, vec![1u8; 200]).unwrap();
    run_kv(
        cache.path(),
        &[
            "put",
            "test",
            &key,
            "--value-from",
            payload_path.to_str().unwrap(),
        ],
        None,
    );
    let stats = run_kv(cache.path(), &["stats"], None);
    assert!(stats.status.success());
    let text = String::from_utf8(stats.stdout).unwrap();
    assert!(text.contains("total_bytes"), "{text}");
    let line = text.lines().next().unwrap();
    let n: u64 = line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .expect("parse total_bytes");
    assert!(n > 0, "total_bytes was zero");
}

#[test]
#[ignore]
fn kv_put_value_from_stdin() {
    let cache = tempfile::tempdir().unwrap();
    let key = hex_key(b"stdin");
    let payload = b"hello-from-stdin".to_vec();
    let put = run_kv(
        cache.path(),
        &["put", "test", &key, "--value-from-stdin"],
        Some(&payload),
    );
    assert!(
        put.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&put.stderr)
    );
    let get = run_kv(cache.path(), &["get", "test", &key], None);
    assert!(get.status.success());
    assert_eq!(get.stdout, payload);
}

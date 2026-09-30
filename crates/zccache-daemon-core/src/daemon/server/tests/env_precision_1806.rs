//! #1806: a rustc compile is keyed by the environment it *reads*.
//!
//! Real compiles through the daemon prove both directions of the contract:
//! two compiles that differ in a variable the crate reads via
//! `env!`/`option_env!` must never share an artifact (wrong bytes are a
//! correctness bug), and two that differ only in a variable the crate never
//! reads must share one (a miss is the spurious split the issue removes).

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::super::*;
use super::CacheDirEnvGuard;

struct Crate {
    root: PathBuf,
    deps: PathBuf,
}

fn make_crate(parent: &Path, name: &str, source: &str) -> Crate {
    let root = parent.join(name);
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), source).unwrap();
    let deps = root.join("target/debug/deps");
    std::fs::create_dir_all(&deps).unwrap();
    Crate { root, deps }
}

fn rustc_args(c: &Crate, crate_name: &str) -> Vec<String> {
    vec![
        "--crate-name".to_string(),
        crate_name.to_string(),
        "--edition=2021".to_string(),
        c.root.join("src/lib.rs").to_string_lossy().into_owned(),
        "--crate-type".to_string(),
        "lib".to_string(),
        "--emit=dep-info,metadata,link".to_string(),
        "-C".to_string(),
        "metadata=5c0ffee".to_string(),
        "-C".to_string(),
        "extra-filename=-5c0ffee".to_string(),
        "--out-dir".to_string(),
        c.deps.to_string_lossy().into_owned(),
        "-L".to_string(),
        format!("dependency={}", c.deps.display()),
    ]
}

/// Compile and return `(cached, rlib bytes)`.
async fn compile(
    state: &std::sync::Arc<SharedState>,
    rustc: &Path,
    c: &Crate,
    crate_name: &str,
    env: &[(&str, &str)],
) -> (bool, Vec<u8>) {
    let env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    let response = handle_compile_ephemeral(
        state,
        std::process::id(),
        &c.root,
        rustc,
        &rustc_args(c, crate_name),
        &c.root,
        Some(env),
        Vec::new(),
    )
    .await;
    let cached = match response {
        Response::CompileResult {
            exit_code,
            cached,
            stderr,
            ..
        } => {
            assert_eq!(exit_code, 0, "{}", String::from_utf8_lossy(&stderr));
            cached
        }
        other => panic!("expected CompileResult, got {other:?}"),
    };
    assert!(
        pending_writes::await_all(&state.pending_cache_writes, Duration::from_secs(30)).await,
        "the compile's artifact must publish"
    );
    let rlib = std::fs::read(c.deps.join(format!("lib{crate_name}-5c0ffee.rlib"))).unwrap();
    (cached, rlib)
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

fn toolchain_rustc() -> PathBuf {
    std::env::var_os("CARGO")
        .map(|cargo| Path::new(&cargo).with_file_name("rustc"))
        .filter(|rustc| rustc.is_file())
        .or_else(|| crate::test_support::find_rustc().map(|rustc| rustc.into_path_buf()))
        .expect("this regression needs the running toolchain's rustc")
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn read_cargo_env_never_shares_an_artifact_and_unread_env_always_does() {
    let rustc = toolchain_rustc();
    let tmp = tempfile::tempdir().unwrap();
    let cache_root: crate::core::NormalizedPath = tmp.path().join("zccache-cache").into();
    let _env_lock = CacheDirEnvGuard::lock();
    let server =
        DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &cache_root)
            .unwrap();
    let state = &server.state;

    // Reads CARGO_PKG_DESCRIPTION via env!() and CARGO_PKG_HOMEPAGE via
    // option_env!(): both must split, including set-vs-unset.
    let reader = make_crate(
        tmp.path(),
        "reader",
        "pub const D: &str = env!(\"CARGO_PKG_DESCRIPTION\");\n\
         pub const H: Option<&str> = option_env!(\"CARGO_PKG_HOMEPAGE\");\n",
    );
    let (cached, rlib) = compile(
        state,
        &rustc,
        &reader,
        "reader",
        &[
            ("CARGO_PKG_DESCRIPTION", "descr-alpha"),
            ("CARGO_PKG_HOMEPAGE", "home-one"),
        ],
    )
    .await;
    assert!(!cached, "first compile is cold");
    assert!(contains(&rlib, "descr-alpha") && contains(&rlib, "home-one"));

    let (cached, rlib) = compile(
        state,
        &rustc,
        &reader,
        "reader",
        &[
            ("CARGO_PKG_DESCRIPTION", "descr-beta"),
            ("CARGO_PKG_HOMEPAGE", "home-one"),
        ],
    )
    .await;
    assert!(!cached, "a changed env!() value must not hit");
    assert!(contains(&rlib, "descr-beta") && !contains(&rlib, "descr-alpha"));

    let (cached, rlib) = compile(
        state,
        &rustc,
        &reader,
        "reader",
        &[("CARGO_PKG_DESCRIPTION", "descr-beta")],
    )
    .await;
    assert!(
        !cached,
        "option_env!() going from set to unset must not hit"
    );
    assert!(!contains(&rlib, "home-one"));

    let (cached, rlib) = compile(
        state,
        &rustc,
        &reader,
        "reader",
        &[
            ("CARGO_PKG_DESCRIPTION", "descr-alpha"),
            ("CARGO_PKG_HOMEPAGE", "home-one"),
        ],
    )
    .await;
    assert!(cached, "returning to a previously built env must hit");
    assert!(contains(&rlib, "descr-alpha") && contains(&rlib, "home-one"));
    assert!(!contains(&rlib, "descr-beta"));

    // Reads none of them: any value of any CARGO_PKG_* shares one artifact.
    let bystander = make_crate(tmp.path(), "bystander", "pub const N: u32 = 41;\n");
    let (cached, _) = compile(
        state,
        &rustc,
        &bystander,
        "bystander",
        &[
            ("CARGO_PKG_DESCRIPTION", "descr-alpha"),
            ("CARGO_PKG_AUTHORS", "a@example"),
        ],
    )
    .await;
    assert!(!cached, "first compile is cold");
    let (cached, _) = compile(
        state,
        &rustc,
        &bystander,
        "bystander",
        &[
            ("CARGO_PKG_DESCRIPTION", "descr-beta"),
            ("CARGO_PKG_AUTHORS", "b@example"),
            ("CARGO_PKG_VERSION", "9.9.9"),
        ],
    )
    .await;
    assert!(
        cached,
        "variables the crate never reads must not split its artifact"
    );
}

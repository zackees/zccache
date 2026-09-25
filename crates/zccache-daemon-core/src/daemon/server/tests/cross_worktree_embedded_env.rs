//! A rustc env-dep value can be embedded in the crate it produces.
//!
//! `env!("CARGO_MANIFEST_DIR")` compiles the worktree's absolute path into
//! the crate, and path remapping never rewrites a string that `env!` read.
//! A sibling worktree with identical sources must therefore compile its own
//! crate, never receive one that names the first worktree.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::super::*;
use super::CacheDirEnvGuard;

struct Worktree {
    root: PathBuf,
    deps: PathBuf,
}

fn worktree(parent: &Path, name: &str) -> Worktree {
    let root = parent.join(name);
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub const MANIFEST_DIR: &str = env!(\"CARGO_MANIFEST_DIR\");\n",
    )
    .unwrap();
    let deps = root.join("target/debug/deps");
    std::fs::create_dir_all(&deps).unwrap();
    Worktree { root, deps }
}

fn rustc_args(tree: &Worktree) -> Vec<String> {
    vec![
        "--crate-name".to_string(),
        "embedsdir".to_string(),
        "--edition=2021".to_string(),
        tree.root.join("src/lib.rs").to_string_lossy().into_owned(),
        "--crate-type".to_string(),
        "lib".to_string(),
        "--emit=dep-info,metadata,link".to_string(),
        "-C".to_string(),
        "metadata=5c0ffee".to_string(),
        "-C".to_string(),
        "extra-filename=-5c0ffee".to_string(),
        "--out-dir".to_string(),
        tree.deps.to_string_lossy().into_owned(),
        "-L".to_string(),
        format!("dependency={}", tree.deps.display()),
    ]
}

async fn compile(state: &std::sync::Arc<SharedState>, rustc: &Path, tree: &Worktree) -> bool {
    let env = vec![
        ("ZCCACHE_PATH_REMAP".to_string(), "auto".to_string()),
        (
            "CARGO_MANIFEST_DIR".to_string(),
            tree.root.to_string_lossy().into_owned(),
        ),
    ];
    let response = handle_compile_ephemeral(
        state,
        std::process::id(),
        &tree.root,
        rustc,
        &rustc_args(tree),
        &tree.root,
        Some(env),
        Vec::new(),
    )
    .await;
    match response {
        Response::CompileResult {
            exit_code,
            cached,
            stderr,
            ..
        } => {
            assert_eq!(
                exit_code,
                0,
                "compile in {} failed: {}",
                tree.root.display(),
                String::from_utf8_lossy(&stderr)
            );
            cached
        }
        other => panic!("expected CompileResult, got {other:?}"),
    }
}

fn embeds(artifact: &Path, root: &Path) -> bool {
    let bytes = std::fs::read(artifact).unwrap();
    let needle = root.to_string_lossy();
    bytes
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_sibling_worktree_never_receives_a_crate_embedding_the_first_manifest_dir() {
    // The toolchain running this test; a PATH `rustc` may be a version shim.
    let rustc = std::env::var_os("CARGO")
        .map(|cargo| Path::new(&cargo).with_file_name("rustc"))
        .filter(|rustc| rustc.is_file())
        .or_else(|| crate::test_support::find_rustc().map(|rustc| rustc.into_path_buf()))
        .expect("this regression needs the running toolchain's rustc");
    let tmp = tempfile::tempdir().unwrap();
    let cache_root: crate::core::NormalizedPath = tmp.path().join("zccache-cache").into();
    let _env_lock = CacheDirEnvGuard::lock();
    let server =
        DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &cache_root)
            .unwrap();
    let first = worktree(tmp.path(), "first-worktree");
    let second = worktree(tmp.path(), "second-worktree");

    assert!(
        !compile(&server.state, &rustc, &first).await,
        "first compile is cold"
    );
    assert!(
        pending_writes::await_all(&server.state.pending_cache_writes, Duration::from_secs(30))
            .await,
        "the compile's artifact must publish"
    );
    let depinfo = std::fs::read_to_string(first.deps.join("embedsdir-5c0ffee.d")).unwrap();
    assert!(
        depinfo.contains("# env-dep:CARGO_MANIFEST_DIR="),
        "the crate must record CARGO_MANIFEST_DIR as an env-dep: {depinfo}"
    );
    let first_rlib = first.deps.join("libembedsdir-5c0ffee.rlib");
    assert!(
        embeds(&first_rlib, &first.root),
        "the crate embeds its manifest dir"
    );

    compile(&server.state, &rustc, &second).await;
    let second_rlib = second.deps.join("libembedsdir-5c0ffee.rlib");
    assert!(
        !embeds(&second_rlib, &first.root),
        "the sibling worktree received a crate embedding the first worktree's path"
    );
    assert!(embeds(&second_rlib, &second.root));
}

//! Cross-worktree sharing for C/C++ compiles that ask for a user depfile.
//!
//! A CMake/Ninja compile passes `-MD -MT <obj> -MF <obj>.d` with absolute
//! `-I` and source paths inside the worktree. Two sibling worktrees with
//! identical sources must share the cached object under
//! `ZCCACHE_PATH_REMAP=auto`, and the depfile restored into the second
//! worktree must name that worktree's files, never the first one's.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::super::*;
use super::CacheDirEnvGuard;

struct Worktree {
    root: PathBuf,
    build: PathBuf,
    source: PathBuf,
    include: PathBuf,
}

fn worktree(parent: &Path, name: &str) -> Worktree {
    let root = parent.join(name);
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let include = root.join("include");
    let source_dir = root.join("src");
    let build = root.join("target/build");
    std::fs::create_dir_all(&include).unwrap();
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::create_dir_all(build.join("obj")).unwrap();
    std::fs::write(include.join("shared.h"), "#define SHARED_VALUE 7\n").unwrap();
    let source = source_dir.join("unit.c");
    std::fs::write(
        &source,
        "#include \"shared.h\"\nint shared_value(void) { return SHARED_VALUE; }\n",
    )
    .unwrap();
    Worktree {
        root,
        build,
        source,
        include,
    }
}

fn ninja_args(tree: &Worktree) -> Vec<String> {
    vec![
        format!("-I{}", tree.include.display()),
        "-O2".to_string(),
        "-MD".to_string(),
        "-MT".to_string(),
        "obj/unit.c.o".to_string(),
        "-MF".to_string(),
        "obj/unit.c.o.d".to_string(),
        "-o".to_string(),
        "obj/unit.c.o".to_string(),
        "-c".to_string(),
        tree.source.to_string_lossy().into_owned(),
    ]
}

async fn compile(state: &std::sync::Arc<SharedState>, cc: &Path, tree: &Worktree) -> bool {
    let env = vec![("ZCCACHE_PATH_REMAP".to_string(), "auto".to_string())];
    let response = handle_compile_ephemeral(
        state,
        std::process::id(),
        &tree.build,
        cc,
        &ninja_args(tree),
        &tree.build,
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

#[cfg(target_os = "linux")]
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn user_depfile_compile_hits_a_sibling_worktree_and_names_its_paths() {
    let cc = Path::new("/usr/bin/gcc");
    if !cc.is_file() {
        panic!("this regression needs the host gcc at {}", cc.display());
    }
    let tmp = tempfile::tempdir().unwrap();
    let cache_root: crate::core::NormalizedPath = tmp.path().join("zccache-cache").into();
    let _env_lock = CacheDirEnvGuard::lock();
    let server =
        DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &cache_root)
            .unwrap();
    let first = worktree(tmp.path(), "first-worktree");
    let second = worktree(tmp.path(), "second-worktree");

    assert!(
        !compile(&server.state, cc, &first).await,
        "first compile is cold"
    );
    assert!(
        pending_writes::await_all(&server.state.pending_cache_writes, Duration::from_secs(30))
            .await,
        "the first worktree's artifact must publish"
    );
    let first_depfile = std::fs::read_to_string(first.build.join("obj/unit.c.o.d")).unwrap();
    assert!(first_depfile.contains(&*first.root.to_string_lossy()));

    let hit = compile(&server.state, cc, &second).await;
    let second_depfile = std::fs::read_to_string(second.build.join("obj/unit.c.o.d")).unwrap();
    assert!(
        hit,
        "the sibling worktree's identical compile must hit the shared entry"
    );
    let first_root = first.root.to_string_lossy();
    let second_root = second.root.to_string_lossy();
    assert!(
        !second_depfile.contains(&*first_root),
        "restored depfile names the other worktree: {second_depfile}"
    );
    assert!(
        second_depfile.contains(&format!("{second_root}/include/shared.h"))
            && second_depfile.contains(&format!("{second_root}/src/unit.c"))
            && second_depfile.starts_with("obj/unit.c.o:"),
        "restored depfile must name this worktree's inputs: {second_depfile}"
    );
    assert!(second.build.join("obj/unit.c.o").is_file());
}

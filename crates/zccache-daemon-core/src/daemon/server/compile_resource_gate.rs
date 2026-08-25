//! Build-wide admission for unusually memory-intensive compiler units.
//!
//! C libraries such as SQLite ship as one multi-megabyte translation unit.
//! Published Rust crates can have the same shape without a large root file:
//! zccache's release crate folds its internal workspace into one rustc unit.
//! Ordinary compiles take shared admission; either kind of amalgamation takes
//! exclusive admission immediately before the compiler child is spawned.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

use crate::compiler::CompilerFamily;

use super::SharedState;

const AMALGAMATION_BYTES: u64 = 1_000_000;
const KNOWN_C_AMALGAMATIONS: &[&str] = &["sqlite3.c", "zstd.c", "rocksdb.cc"];
const KNOWN_RUST_AMALGAMATIONS: &[&str] = &["zccache"];

/// Fair shared/exclusive admission around real compiler execution.
///
/// Tokio's write-preferring lock prevents newly arriving ordinary compiles
/// from starving an amalgamation after it reaches the queue.
#[derive(Clone, Default)]
pub(super) struct CompileResourceGate {
    inner: Arc<RwLock<()>>,
}

pub(super) enum CompileResourcePermit {
    Shared { _guard: OwnedRwLockReadGuard<()> },
    Exclusive { _guard: OwnedRwLockWriteGuard<()> },
}

/// Owns both layers of compiler admission in their canonical lock order.
///
/// The bounded FIFO semaphore comes first, then the fair resource gate. This
/// prevents ordinary readers from accumulating ahead of an exclusive unit
/// while also ensuring nobody holds the resource gate while waiting for a
/// scarce compile slot.
pub(super) struct CompilerAdmission {
    _resource: CompileResourcePermit,
    _compile: super::compile_progress::CompileGateGuard,
}

impl CompileResourceGate {
    pub(super) async fn acquire(&self, exclusive: bool) -> CompileResourcePermit {
        if exclusive {
            CompileResourcePermit::Exclusive {
                _guard: Arc::clone(&self.inner).write_owned().await,
            }
        } else {
            CompileResourcePermit::Shared {
                _guard: Arc::clone(&self.inner).read_owned().await,
            }
        }
    }
}

pub(super) async fn acquire_compiler_admission(
    state: &SharedState,
    exclusive: bool,
) -> (CompilerAdmission, Option<usize>) {
    let (compile, available_before) = super::compile_progress::acquire_compile_gate(
        state.compile_concurrency.as_ref(),
        &state.compile_queue,
    )
    .await;
    let resource = state.compile_resource_gate.acquire(exclusive).await;
    (
        CompilerAdmission {
            _resource: resource,
            _compile: compile,
        },
        available_before,
    )
}

/// Decide whether one compiler invocation must run without sibling compilers.
pub(super) fn requires_exclusive_access(
    family: CompilerFamily,
    args: &[String],
    source_path: &Path,
) -> bool {
    match family {
        CompilerFamily::Rustc => is_known_rust_amalgamation(args),
        CompilerFamily::Gcc | CompilerFamily::Clang | CompilerFamily::Msvc => {
            let known = source_path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| KNOWN_C_AMALGAMATIONS.contains(&name));
            known
                || std::fs::metadata(source_path)
                    .is_ok_and(|metadata| metadata.len() >= AMALGAMATION_BYTES)
        }
        CompilerFamily::Rustfmt => false,
    }
}

fn is_known_rust_amalgamation(args: &[String]) -> bool {
    rust_crate_name(args).is_some_and(|name| KNOWN_RUST_AMALGAMATIONS.contains(&name))
        && rust_crate_types_are_non_linking(args)
}

fn rust_crate_types_are_non_linking(args: &[String]) -> bool {
    if args.iter().any(|arg| arg == "--test") {
        return false;
    }

    let mut saw_crate_type = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let value = if arg == "--crate-type" {
            let Some(value) = args.next() else {
                return false;
            };
            value.as_str()
        } else if let Some(value) = arg.strip_prefix("--crate-type=") {
            value
        } else {
            continue;
        };

        for crate_type in value.split(',') {
            saw_crate_type = true;
            if !matches!(crate_type, "lib" | "rlib") {
                return false;
            }
        }
    }
    saw_crate_type
}

fn rust_crate_name(args: &[String]) -> Option<&str> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--crate-name" {
            return args.next().map(String::as_str);
        }
        if let Some(name) = arg.strip_prefix("--crate-name=") {
            return Some(name);
        }
    }
    None
}

/// Classify an invocation when parsing fell back to direct execution.
pub(super) fn requires_exclusive_access_from_args(
    family: CompilerFamily,
    args: &[String],
    cwd: &Path,
) -> bool {
    if family == CompilerFamily::Rustc {
        return is_known_rust_amalgamation(args);
    }
    if !matches!(
        family,
        CompilerFamily::Gcc | CompilerFamily::Clang | CompilerFamily::Msvc
    ) {
        return false;
    }
    args.iter()
        .filter(|arg| {
            !(arg.starts_with('-') || family == CompilerFamily::Msvc && arg.starts_with('/'))
        })
        .filter_map(|arg| {
            let path = Path::new(arg);
            let extension = path.extension()?.to_str()?.to_ascii_lowercase();
            matches!(
                extension.as_str(),
                "c" | "cc" | "cpp" | "cxx" | "c++" | "m" | "mm"
            )
            .then_some(path)
        })
        .any(|path| {
            let source = if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            };
            requires_exclusive_access(family, args, &source)
        })
}

pub(super) fn requires_exclusive_access_for_misses<'a>(
    family: CompilerFamily,
    args: &[String],
    sources: impl IntoIterator<Item = Option<&'a Path>>,
) -> bool {
    sources
        .into_iter()
        .flatten()
        .any(|source| requires_exclusive_access(family, args, source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_published_zccache_crate_is_a_rust_amalgamation() {
        assert!(requires_exclusive_access(
            CompilerFamily::Rustc,
            &[
                "--crate-name".into(),
                "zccache".into(),
                "--crate-type=lib".into(),
            ],
            Path::new("/registry/zccache/src/lib.rs"),
        ));
    }

    #[test]
    fn a_known_rust_binary_remains_shared_for_nested_linker_calls() {
        assert!(!requires_exclusive_access(
            CompilerFamily::Rustc,
            &["--crate-name=zccache".into(), "--crate-type=bin".into(),],
            Path::new("/registry/zccache/src/main.rs"),
        ));
    }

    #[test]
    fn a_known_rust_test_harness_remains_shared_for_nested_linker_calls() {
        assert!(!requires_exclusive_access(
            CompilerFamily::Rustc,
            &[
                "--crate-name=zccache".into(),
                "--crate-type=lib".into(),
                "--test".into(),
            ],
            Path::new("/registry/zccache/src/lib.rs"),
        ));
    }

    #[test]
    fn explicit_rlib_forms_are_exclusive() {
        for crate_type_args in [
            vec!["--crate-type=rlib".into()],
            vec!["--crate-type".into(), "rlib".into()],
            vec!["--crate-type=lib,rlib".into()],
        ] {
            let mut args = vec!["--crate-name=zccache".into()];
            args.extend(crate_type_args);
            assert!(requires_exclusive_access(
                CompilerFamily::Rustc,
                &args,
                Path::new("/registry/zccache/src/lib.rs"),
            ));
        }
    }

    #[test]
    fn any_linking_crate_type_keeps_a_mixed_invocation_shared() {
        for crate_types in ["lib,bin", "bin,lib", "lib,cdylib", "cdylib,lib"] {
            assert!(!requires_exclusive_access(
                CompilerFamily::Rustc,
                &[
                    "--crate-name=zccache".into(),
                    format!("--crate-type={crate_types}"),
                ],
                Path::new("/registry/zccache/src/lib.rs"),
            ));
        }
    }

    #[test]
    fn an_ordinary_rust_crate_remains_shared() {
        assert!(!requires_exclusive_access(
            CompilerFamily::Rustc,
            &["--crate-name=serde".into()],
            Path::new("/registry/serde/src/lib.rs"),
        ));
    }

    #[test]
    fn an_oversized_private_c_unit_is_detected_by_shape() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("private-amalgamation.c");
        std::fs::write(&source, vec![b'x'; AMALGAMATION_BYTES as usize])
            .expect("write source fixture");

        assert!(requires_exclusive_access(
            CompilerFamily::Clang,
            &[],
            &source,
        ));
    }

    #[test]
    fn a_small_known_c_amalgamation_is_still_recognized() {
        assert!(requires_exclusive_access(
            CompilerFamily::Gcc,
            &[],
            Path::new("sqlite3.c"),
        ));
    }

    #[tokio::test]
    async fn exclusive_admission_drains_shared_work_and_blocks_new_readers() {
        use std::time::Duration;

        let gate = CompileResourceGate::default();
        let first_shared = gate.acquire(false).await;

        let writer_gate = gate.clone();
        let (writer_acquired_tx, mut writer_acquired_rx) = tokio::sync::oneshot::channel();
        let (release_writer_tx, release_writer_rx) = tokio::sync::oneshot::channel();
        let writer = tokio::spawn(async move {
            let _permit = writer_gate.acquire(true).await;
            let _ = writer_acquired_tx.send(());
            let _ = release_writer_rx.await;
        });
        tokio::task::yield_now().await;

        let later_reader_gate = gate.clone();
        let (reader_acquired_tx, mut reader_acquired_rx) = tokio::sync::oneshot::channel();
        let reader = tokio::spawn(async move {
            let _permit = later_reader_gate.acquire(false).await;
            let _ = reader_acquired_tx.send(());
        });

        assert!(writer_acquired_rx.try_recv().is_err());
        assert!(reader_acquired_rx.try_recv().is_err());
        drop(first_shared);

        tokio::time::timeout(Duration::from_secs(1), &mut writer_acquired_rx)
            .await
            .expect("writer should acquire after the active reader drains")
            .expect("writer task should report acquisition");
        assert!(
            reader_acquired_rx.try_recv().is_err(),
            "a queued writer must block later readers"
        );

        let _ = release_writer_tx.send(());
        tokio::time::timeout(Duration::from_secs(1), reader_acquired_rx)
            .await
            .expect("reader should resume after exclusive work")
            .expect("reader task should report acquisition");
        writer.await.expect("writer task");
        reader.await.expect("reader task");
    }

    #[tokio::test]
    async fn ordinary_units_can_hold_shared_admission_together() {
        use std::time::Duration;

        let gate = CompileResourceGate::default();
        let _first = gate.acquire(false).await;
        tokio::time::timeout(Duration::from_secs(1), gate.acquire(false))
            .await
            .expect("ordinary compiles must not serialize");
    }

    #[test]
    fn direct_rust_and_c_invocations_use_the_same_classifier() {
        assert!(requires_exclusive_access_from_args(
            CompilerFamily::Rustc,
            &[
                "--crate-name=zccache".into(),
                "--crate-type=lib".into(),
                "src/lib.rs".into(),
            ],
            Path::new("."),
        ));
        assert!(requires_exclusive_access_from_args(
            CompilerFamily::Clang,
            &["-c".into(), "sqlite3.c".into()],
            Path::new("."),
        ));
    }

    #[test]
    fn cached_amalgamation_does_not_make_an_ordinary_multi_source_miss_exclusive() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sqlite = temp.path().join("sqlite3.c");
        let ordinary = temp.path().join("ordinary.c");
        std::fs::write(&sqlite, "/* cached */").expect("write sqlite fixture");
        std::fs::write(&ordinary, "int ordinary(void) { return 0; }")
            .expect("write ordinary fixture");

        let args = vec!["-c".to_string()];
        assert!(!requires_exclusive_access_for_misses(
            CompilerFamily::Gcc,
            &args,
            [None, Some(ordinary.as_path())],
        ));
        assert!(requires_exclusive_access_for_misses(
            CompilerFamily::Gcc,
            &args,
            [Some(sqlite.as_path()), Some(ordinary.as_path())],
        ));
    }

    #[test]
    fn cache_hit_branches_precede_compiler_admission() {
        let pipeline = include_str!("handle_compile/pipeline/mod.rs");
        let compile_miss = pipeline
            .find("run_compile_exec(CompileExecRequest")
            .expect("pipeline must dispatch real cache misses");
        for hit in [
            "try_request_cache_hit(",
            "try_fast_hit(",
            "try_depgraph_cached_hit(",
        ] {
            let last_hit = pipeline
                .rfind(hit)
                .unwrap_or_else(|| panic!("pipeline must retain its {hit} branch"));
            assert!(
                last_hit < compile_miss,
                "{hit} must return before compiler admission is reachable"
            );
        }
    }

    #[test]
    fn every_compile_spawn_surface_uses_shared_admission() {
        for (name, source) in [
            (
                "single cache miss",
                include_str!("handle_compile/pipeline/compile_exec.rs"),
            ),
            (
                "direct fallback",
                include_str!("handle_compile_ephemeral.rs"),
            ),
            (
                "legacy multi-source",
                include_str!("handle_compile_multi.rs"),
            ),
            (
                "staged multi-source",
                include_str!("handle_compile_multi_staged.rs"),
            ),
        ] {
            assert!(
                source.contains("acquire_compiler_admission("),
                "{name} compiler path bypasses shared admission"
            );
        }
    }

    #[test]
    fn bounded_compile_admission_precedes_the_resource_gate() {
        let source = include_str!("compile_resource_gate.rs");
        let compile = source
            .find("compile_progress::acquire_compile_gate(")
            .expect("shared admission must acquire the compile gate");
        let resource = source
            .find("compile_resource_gate.acquire(exclusive)")
            .expect("shared admission must acquire the resource gate");
        assert!(
            compile < resource,
            "canonical lock order must remain stable"
        );
    }
}

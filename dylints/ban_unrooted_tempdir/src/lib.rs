#![feature(rustc_private)]

extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_span;

use rustc_errors::DiagDecorator;
use rustc_hir::{def::Res, Expr, ExprKind};
use rustc_lint::{LateContext, LateLintPass, LintContext};
use rustc_span::{symbol::Symbol, FileName, RemapPathScopeComponents};

dylint_linting::declare_late_lint! {
    /// ### What it does
    ///
    /// Bans calls that create scratch directories/files under the OS temp
    /// directory (`$TMPDIR` / `%TEMP%`) instead of under
    /// `zccache_core::config::default_cache_dir()`.
    ///
    /// ### Why is this bad?
    ///
    /// zccache state should live under one ground-truth directory the user
    /// can inspect or override via `ZCCACHE_CACHE_DIR`. Scratch dirs
    /// scattered across `$TMPDIR` are invisible to `zccache clear`, survive
    /// process death on Windows for hours, and on Windows specifically
    /// can sit on a different volume from the destination — breaking the
    /// atomic-rename invariant that `tempfile::NamedTempFile::persist`
    /// relies on.
    ///
    /// ### Known problems
    ///
    /// Legacy call sites are exempted via `src/allowlist.txt`. Migrate them
    /// as you touch each file.
    ///
    /// ### Example
    ///
    /// ```rust
    /// let dir = tempfile::tempdir().unwrap();
    /// ```
    ///
    /// Use instead:
    ///
    /// ```rust
    /// let root = zccache_core::config::tmp_dir();
    /// std::fs::create_dir_all(&root).unwrap();
    /// let dir = tempfile::tempdir_in(&root).unwrap();
    /// ```
    pub BAN_UNROOTED_TEMPDIR,
    Deny,
    "ban tempdir/temp_dir calls that aren't rooted under zccache's cache dir"
}

/// Each entry is a fully-qualified path that resolves to a banned function
/// or associated function. Matching is exact — sub-paths are not banned.
/// The `*_in(...)` variants (`tempdir_in`, `TempDir::new_in`,
/// `NamedTempFile::new_in`) are intentionally absent: they accept an
/// explicit base directory and are the recommended replacement.
const BANNED_FN_PATHS: &[&[&str]] = &[
    &["std", "env", "temp_dir"],
    &["tempfile", "tempdir"],
    &["tempfile", "dir", "TempDir", "new"],
    &["tempfile", "TempDir", "new"],
    &["tempfile", "file", "NamedTempFile", "new"],
    &["tempfile", "NamedTempFile", "new"],
];

const ALLOWLIST: &str = include_str!("allowlist.txt");

impl<'tcx> LateLintPass<'tcx> for BanUnrootedTempdir {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        if is_allowlisted(cx, expr.span) {
            return;
        }

        if let ExprKind::Path(qpath) = expr.kind {
            let res = cx.qpath_res(&qpath, expr.hir_id);
            if let Res::Def(_, def_id) = res {
                for banned in BANNED_FN_PATHS {
                    if def_path_equals(cx, def_id, banned) {
                        emit_lint(cx, expr.span, banned);
                        return;
                    }
                }
            }
        }
    }
}

fn emit_lint(cx: &LateContext<'_>, span: rustc_span::Span, banned: &[&str]) {
    let joined = banned.join("::");
    cx.opt_span_lint(
        BAN_UNROOTED_TEMPDIR,
        Some(span),
        DiagDecorator(move |diag| {
            diag.primary_message(format!(
                "`{joined}` writes under $TMPDIR; root it under zccache_core::config::default_cache_dir() (e.g. tmp_dir() or symbols_cache_dir()) and use the `_in` variant"
            ));
        }),
    );
}

fn is_allowlisted(cx: &LateContext<'_>, span: rustc_span::Span) -> bool {
    let filename = match cx.sess().source_map().span_to_filename(span) {
        FileName::Real(real_filename) => real_filename
            .local_path()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| {
                real_filename
                    .path(RemapPathScopeComponents::DIAGNOSTICS)
                    .to_string_lossy()
                    .into_owned()
            }),
        filename => filename
            .display(RemapPathScopeComponents::DIAGNOSTICS)
            .to_string(),
    };
    let normalized = normalize_slashes(&filename);

    // Blanket-allow integration tests and benchmarks. These run on the
    // developer's machine, not in the user's installed binary, so they don't
    // need to land under `~/.zccache/`. The lint is about production code
    // shipping in `zccache.exe` / `zccache-daemon.exe`.
    if normalized.contains("/tests/") || normalized.contains("/benches/") {
        return true;
    }

    ALLOWLIST
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .any(|allowed| normalized.ends_with(allowed))
}

fn normalize_slashes(path: &str) -> String {
    path.replace('\\', "/")
}

fn def_path_equals(cx: &LateContext<'_>, def_id: rustc_hir::def_id::DefId, expected: &[&str]) -> bool {
    let def_path = cx.get_def_path(def_id);
    if def_path.len() != expected.len() {
        return false;
    }
    def_path
        .iter()
        .zip(expected.iter())
        .all(|(actual, expected_segment)| *actual == Symbol::intern(expected_segment))
}

#[cfg(test)]
struct CurrentDirGuard(std::path::PathBuf);

#[cfg(test)]
impl CurrentDirGuard {
    fn set(path: &std::path::Path) -> Self {
        let previous = std::env::current_dir().expect("current dir should be readable");
        std::env::set_current_dir(path).expect("current dir should switch to manifest dir");
        Self(previous)
    }
}

#[cfg(test)]
fn prepare_dylint_library() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let status = std::process::Command::new("cargo")
        .arg("build")
        .current_dir(manifest_dir)
        .status()
        .expect("cargo build should start");
    assert!(status.success(), "cargo build should succeed");

    let toolchain = std::env::var("RUSTUP_TOOLCHAIN").expect("RUSTUP_TOOLCHAIN should be set");
    let library_name = env!("CARGO_PKG_NAME").replace('-', "_");
    let target_debug = manifest_dir.join("target").join("debug");
    let expected = target_debug.join(format!(
        "{}{}@{}{}",
        std::env::consts::DLL_PREFIX,
        library_name,
        toolchain,
        std::env::consts::DLL_SUFFIX
    ));
    if expected.exists() {
        return;
    }

    let plain = target_debug.join(format!(
        "{}{}{}",
        std::env::consts::DLL_PREFIX,
        library_name,
        std::env::consts::DLL_SUFFIX
    ));
    if plain.exists() {
        std::fs::copy(&plain, &expected)
            .expect("toolchain-suffixed dylint library should be copied");
        return;
    }

    let deps_dir = target_debug.join("deps");
    for entry in std::fs::read_dir(&deps_dir).expect("deps dir should be readable") {
        let path = entry.expect("deps entry should be readable").path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if name.starts_with(&format!("{}{}", std::env::consts::DLL_PREFIX, library_name))
            && name.ends_with(std::env::consts::DLL_SUFFIX)
        {
            std::fs::copy(&path, &expected)
                .expect("hashed dylint library should be copied to the expected filename");
            return;
        }
    }

    panic!(
        "could not find a built dylint library to copy into {}",
        expected.display()
    );
}

#[cfg(test)]
impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.0).expect("current dir should be restored");
    }
}

#[test]
fn ui() {
    let _guard = CurrentDirGuard::set(std::path::Path::new(env!("CARGO_MANIFEST_DIR")));
    prepare_dylint_library();
    dylint_testing::ui_test(env!("CARGO_PKG_NAME"), "ui");
}

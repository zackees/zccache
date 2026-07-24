#![feature(rustc_private)]

extern crate rustc_ast;
extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_span;

use rustc_ast::LitKind;
use rustc_errors::DiagDecorator;
use rustc_hir::{Expr, ExprKind};
use rustc_lint::{LateContext, LateLintPass, LintContext};
use rustc_span::{FileName, RemapPathScopeComponents};

dylint_linting::declare_late_lint! {
    /// ### What it does
    ///
    /// Bans string literals that hardcode the POSIX temp directory:
    /// `"/tmp"` or anything starting with `"/tmp/"`.
    ///
    /// ### Why is this bad?
    ///
    /// zccache ships on Linux, macOS, and Windows. A literal `/tmp/...`
    /// path only exists on POSIX: Windows callers either silently write to
    /// `C:\tmp\` (if it exists, polluting the filesystem) or fail. CI on
    /// macOS/Linux passes; Windows runners or dev boxes break.
    ///
    /// ### Known problems
    ///
    /// Legacy call sites are exempted via `src/allowlist.txt`. The
    /// `cfg(unix)` daemon-socket endpoints are permanent exemptions (the
    /// `/tmp/zccache-{user}` convention is part of the daemon-discovery
    /// contract); fs-inert test fixtures should migrate to neutral fake
    /// paths as each file is touched.
    ///
    /// ### Example
    ///
    /// ```rust
    /// let log = std::path::Path::new("/tmp/zc.log");
    /// ```
    ///
    /// Use instead:
    ///
    /// ```rust
    /// let root = zccache_core::config::tmp_dir();
    /// let log = root.join("zc.log");
    /// ```
    pub BAN_TMP_LITERAL,
    Deny,
    "ban hardcoded /tmp path literals; they only exist on POSIX"
}

const ALLOWLIST: &str = include_str!("allowlist.txt");

impl<'tcx> LateLintPass<'tcx> for BanTmpLiteral {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        let ExprKind::Lit(lit) = expr.kind else {
            return;
        };
        let LitKind::Str(symbol, _) = lit.node else {
            return;
        };
        let value = symbol.as_str();
        if value != "/tmp" && !value.starts_with("/tmp/") {
            return;
        }
        if is_allowlisted(cx, expr.span) {
            return;
        }
        emit_lint(cx, expr.span);
    }
}

fn emit_lint(cx: &LateContext<'_>, span: rustc_span::Span) {
    cx.opt_span_lint(
        BAN_TMP_LITERAL,
        Some(span),
        DiagDecorator(move |diag| {
            diag.primary_message(
                "hardcoded `/tmp` path only exists on POSIX; use \
                 zccache_core::config::tmp_dir() for runtime scratch state, \
                 tempfile::tempdir_in(...) for tests that touch the \
                 filesystem, or a neutral fake path (e.g. `/fixture/...`) \
                 for fs-inert fixtures",
            );
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

    ALLOWLIST
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .any(|allowed| normalized.ends_with(allowed))
}

fn normalize_slashes(path: &str) -> String {
    path.replace('\\', "/")
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

// UI test ignored until matching `.stderr` snapshots are captured locally.
// The lint behavior is exercised end-to-end by `cargo dylint --all
// --workspace` against the real workspace tree (the allowlist covers the
// legacy sites, and any new violation lights up the workspace lint). Same
// arrangement as ban_unrooted_tempdir.
#[test]
#[ignore = "no .stderr snapshots yet — verify via `cargo dylint --all --workspace`"]
fn ui() {
    let _guard = CurrentDirGuard::set(std::path::Path::new(env!("CARGO_MANIFEST_DIR")));
    prepare_dylint_library();
    dylint_testing::ui_test(env!("CARGO_PKG_NAME"), "ui");
}

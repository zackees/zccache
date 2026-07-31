#![feature(rustc_private)]

extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_span;

use rustc_errors::DiagDecorator;
use rustc_hir::intravisit::{self, Visitor};
use rustc_hir::{Expr, ExprKind, PatKind, QPath, Stmt, StmtKind};
use rustc_lint::{LateContext, LateLintPass, LintContext};
use rustc_span::{symbol::Symbol, FileName, RemapPathScopeComponents, Span};

dylint_linting::declare_late_lint! {
    /// ### What it does
    ///
    /// In the daemon's persistence modules, rejects a `Result` produced by a
    /// write-ish call that is thrown away — either `let _ = <write call>;`
    /// or a statement-position `<write call>.ok();`.
    ///
    /// ### Why is this bad?
    ///
    /// This is the #1163 bug class. `miss_store.rs` published artifact-index
    /// records with `let _ = …index_writer_tx.send(…)` and then
    /// unconditionally recorded the artifact as cached. When the send failed
    /// the record never reached `index.bin`, so the next daemon start
    /// re-missed the artifact — with no error anywhere. A dropped
    /// `write`/`rename`/`persist`/`flush` has the same shape: the caller
    /// proceeds as though durable state landed when it did not.
    ///
    /// ### Known problems
    ///
    /// Genuinely fire-and-forget sites (rollback on an already-failing path,
    /// best-effort durability hints, test-orchestration channels) are
    /// exempted per file via `src/allowlist.txt`, each with a comment
    /// justifying the dropped error.
    ///
    /// ### Example
    ///
    /// ```rust,ignore
    /// let _ = index_writer_tx.send(record);
    /// state.artifacts.insert(key, entry);
    /// ```
    ///
    /// Use instead:
    ///
    /// ```rust,ignore
    /// if let Err(error) = index_writer_tx.send(record) {
    ///     tracing::warn!(%error, "artifact index record dropped");
    ///     return Err(error.into());
    /// }
    /// state.artifacts.insert(key, entry);
    /// ```
    pub BAN_DISCARDED_WRITE_RESULT,
    Deny,
    "ban discarding the Result of a write-ish call in daemon persistence code"
}

/// Callee names whose dropped `Result` means durable state may silently not
/// exist. Kept deliberately tight: cleanup-shaped calls (`remove_file`,
/// `remove_dir_all`, `set_readonly`) are *not* listed, because discarding
/// those on an already-failing path is the normal idiom and listing them
/// would drown the real signal. `send` is listed on purpose — it is the
/// #1163 vector.
const WRITE_CALLEE_NAMES: &[&str] = &[
    "write",
    "write_all",
    "send",
    "rename",
    "persist",
    "flush",
    "sync_all",
    "set_len",
];

/// Only the daemon's state-mutation modules are in scope. Every entry is
/// verified to exist by `ci/check_dylint_wiring.py`.
const DAEMON_SOURCE_PREFIXES: &[&str] = &[
    "crates/zccache-daemon-core/src/daemon/server/persist/",
    "crates/zccache-daemon-core/src/daemon/server/handle_compile/miss_store.rs",
    "crates/zccache-daemon-core/src/daemon/server/wal.rs",
];

const ALLOWLIST: &str = include_str!("allowlist.txt");

impl<'tcx> LateLintPass<'tcx> for BanDiscardedWriteResult {
    fn check_stmt(&mut self, cx: &LateContext<'tcx>, stmt: &'tcx Stmt<'tcx>) {
        if !in_scope(cx, stmt.span) {
            return;
        }
        match stmt.kind {
            // Shape 1: `let _ = <expr>;`
            StmtKind::Let(local) => {
                if !matches!(local.pat.kind, PatKind::Wild) {
                    return;
                }
                let Some(init) = local.init else {
                    return;
                };
                if !is_result_ty(cx, init) {
                    return;
                }
                if let Some(name) = find_write_callee(init) {
                    emit_lint(cx, stmt.span, name, "let _ =");
                }
            }
            // Shape 2: statement-position `<expr>.ok();`
            StmtKind::Semi(expr) => {
                let ExprKind::MethodCall(segment, receiver, _, _) = expr.kind else {
                    return;
                };
                if segment.ident.name.as_str() != "ok" {
                    return;
                }
                if !is_result_ty(cx, receiver) {
                    return;
                }
                if let Some(name) = find_write_callee(receiver) {
                    emit_lint(cx, stmt.span, name, ".ok();");
                }
            }
            _ => {}
        }
    }
}

fn emit_lint(cx: &LateContext<'_>, span: Span, callee: Symbol, shape: &str) {
    // Keep the substring "ui" out of this message: compiletest normalizes the
    // UI-fixture directory name and would rewrite it to `$DIR` mid-word.
    let message = format!(
        "`{shape}` discards the `Result` of `{callee}`; durable state may silently not exist. \
         Propagate with `?`, or handle the error and gate the follow-on state mutation on \
         success. If the site is fire-and-forget by design, add its path to \
         `dylints/ban_discarded_write_result/src/allowlist.txt` with a justifying comment"
    );
    cx.opt_span_lint(
        BAN_DISCARDED_WRITE_RESULT,
        Some(span),
        DiagDecorator(move |diag| {
            diag.primary_message(message.clone());
        }),
    );
}

/// Walks the discarded expression looking for a write-ish callee. Nested
/// bodies (closures, async blocks) are not entered — the default
/// `NestedFilter` is `None` — so a discarded outer `Result` is never blamed
/// on a write buried in an unrelated closure.
struct WriteCalleeFinder {
    found: Option<Symbol>,
}

impl<'tcx> Visitor<'tcx> for WriteCalleeFinder {
    fn visit_expr(&mut self, expr: &'tcx Expr<'tcx>) {
        if self.found.is_some() {
            return;
        }
        if let Some(name) = callee_name(expr) {
            if WRITE_CALLEE_NAMES.contains(&&*name.as_str()) {
                self.found = Some(name);
                return;
            }
        }
        intravisit::walk_expr(self, expr);
    }
}

fn find_write_callee<'tcx>(expr: &'tcx Expr<'tcx>) -> Option<Symbol> {
    let mut finder = WriteCalleeFinder { found: None };
    finder.visit_expr(expr);
    finder.found
}

fn callee_name(expr: &Expr<'_>) -> Option<Symbol> {
    match expr.kind {
        ExprKind::MethodCall(segment, ..) => Some(segment.ident.name),
        ExprKind::Call(callee, _) => match callee.kind {
            ExprKind::Path(ref qpath) => last_segment_name(qpath),
            _ => None,
        },
        _ => None,
    }
}

fn last_segment_name(qpath: &QPath<'_>) -> Option<Symbol> {
    match qpath {
        QPath::Resolved(_, path) => path.segments.last().map(|segment| segment.ident.name),
        QPath::TypeRelative(_, segment) => Some(segment.ident.name),
    }
}

fn is_result_ty<'tcx>(cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) -> bool {
    cx.typeck_results()
        .expr_ty(expr)
        .peel_refs()
        .ty_adt_def()
        .is_some_and(|definition| {
            cx.tcx
                .def_path_str(definition.did())
                .ends_with("result::Result")
        })
}

fn in_scope(cx: &LateContext<'_>, span: Span) -> bool {
    let normalized = normalize_slashes(&source_filename(cx, span));

    // UI fixtures live beside this lint rather than beneath the daemon crate.
    // Keeping that narrow exception lets the UI test exercise the real
    // matcher without relaxing the production scope.
    let is_ui_fixture = normalized.starts_with("ui/") || normalized.contains("/ui/");
    if !is_ui_fixture
        && !DAEMON_SOURCE_PREFIXES
            .iter()
            .any(|prefix| normalized.contains(prefix))
    {
        return false;
    }
    if normalized.contains("/tests/") || normalized.ends_with("_tests.rs") {
        return false;
    }
    !is_allowlisted(&normalized)
}

fn is_allowlisted(normalized: &str) -> bool {
    ALLOWLIST
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .any(|allowed| normalized.ends_with(allowed))
}

fn source_filename(cx: &LateContext<'_>, span: Span) -> String {
    match cx.sess().source_map().span_to_filename(span) {
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
    }
}

fn normalize_slashes(path: &str) -> String {
    path.replace('\\', "/")
}

#[cfg(test)]
mod ui_test_support {
    include!("../../ui_test_support.rs");
}

#[test]
fn ui() {
    ui_test_support::run(env!("CARGO_PKG_NAME"), env!("CARGO_MANIFEST_DIR"));
}

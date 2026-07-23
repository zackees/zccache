#![feature(rustc_private)]

extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_middle;
extern crate rustc_span;

use rustc_errors::DiagDecorator;
use rustc_hir::{Expr, ExprKind};
use rustc_lint::{LateContext, LateLintPass, LintContext};
use rustc_middle::ty;
use rustc_span::{FileName, RemapPathScopeComponents, Span};

dylint_linting::declare_late_lint! {
    /// Rejects `NormalizedPath` method syntax when rustc resolves it to a raw
    /// `std::path::Path` containment method through `Deref`.
    pub BAN_NORMALIZED_PATH_DEREF_CONTAINMENT,
    Deny,
    "use NormalizedPath's inherent normalized containment methods"
}

const RAW_METHODS: &[&str] = &[
    "std::path::Path::starts_with",
    "std::path::Path::strip_prefix",
];
const ALLOWLIST: &str = include_str!("allowlist.txt");

impl<'tcx> LateLintPass<'tcx> for BanNormalizedPathDerefContainment {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        let ExprKind::MethodCall(_, receiver, _, _) = expr.kind else {
            return;
        };
        let filename = normalized_filename(cx, expr.span);
        if is_allowlisted(&filename) {
            return;
        }
        let Some(def_id) = cx.typeck_results().type_dependent_def_id(expr.hir_id) else {
            return;
        };
        let resolved = cx.tcx.def_path_str(def_id);
        if !RAW_METHODS.iter().any(|method| resolved.ends_with(method)) {
            return;
        }

        // Deliberately inspect the receiver before method adjustments. The
        // adjusted type is `&Path` after autoderef and loses the evidence that
        // the call originated on NormalizedPath.
        let receiver_type = cx.typeck_results().expr_ty(receiver).peel_refs();
        let ty::Adt(receiver_adt, _) = receiver_type.kind() else {
            return;
        };
        let receiver_path = cx.tcx.def_path_str(receiver_adt.did());
        let is_workspace_type = receiver_path.ends_with("zccache_core::path::NormalizedPath");
        let is_ui_fixture = (filename.starts_with("ui/") || filename.contains("/ui/"))
            && receiver_path.ends_with("NormalizedPath");
        if !is_workspace_type && !is_ui_fixture {
            return;
        }

        cx.opt_span_lint(
            BAN_NORMALIZED_PATH_DEREF_CONTAINMENT,
            Some(expr.span),
            DiagDecorator(|diag| {
                diag.primary_message(
                    "raw Path containment bypasses NormalizedPath identity semantics; \
                     call the inherent NormalizedPath method",
                );
            }),
        );
    }
}

fn normalized_filename(cx: &LateContext<'_>, span: Span) -> String {
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
    filename.replace('\\', "/")
}

fn is_allowlisted(filename: &str) -> bool {
    ALLOWLIST
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .any(|allowed| filename.ends_with(allowed))
}

#[cfg(test)]
mod ui_test_support {
    include!("../../ui_test_support.rs");
}

#[test]
fn ui() {
    ui_test_support::run(env!("CARGO_PKG_NAME"), env!("CARGO_MANIFEST_DIR"));
}

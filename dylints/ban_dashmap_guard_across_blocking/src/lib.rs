#![feature(rustc_private)]

extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_span;

use rustc_errors::DiagDecorator;
use rustc_hir::{Expr, ExprKind};
use rustc_lint::LintContext;

dylint_linting::declare_late_lint! {
    /// ### What it does
    ///
    /// Rejects a direct `if let Some(guard) = map.get(..)` whose body waits,
    /// performs filesystem/process work, or mutates a map.
    ///
    /// ### Why is this bad?
    ///
    /// A DashMap guard holds a shard lock. Keeping it across work that can
    /// block permits another operation on that shard to deadlock.
    ///
    /// ### Example
    ///
    /// ```rust,ignore
    /// if let Some(entry) = cache.get(&key) {
    ///     materialize(entry.value()).await?;
    /// }
    /// ```
    ///
    /// Clone the entry first, so the guard is dropped before the blocking
    /// operation.
    pub BAN_DASHMAP_GUARD_ACROSS_BLOCKING,
    Deny,
    "DashMap guard is held across blocking work"
}

impl<'tcx> rustc_lint::LateLintPass<'tcx> for BanDashmapGuardAcrossBlocking {
    fn check_expr(&mut self, cx: &rustc_lint::LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        let ExprKind::If(condition, body, _) = expr.kind else {
            return;
        };
        let Some(receiver) = dashmap_get_receiver(cx, condition) else {
            return;
        };
        if !body_can_block_or_reenter(cx, body, &receiver) {
            return;
        }
        cx.opt_span_lint(
            BAN_DASHMAP_GUARD_ACROSS_BLOCKING,
            Some(condition.span),
            DiagDecorator(|diag| {
                diag.primary_message(
                    "a DashMap guard is live for this whole `if let` body; clone the entry before blocking or mutating the map",
                );
            }),
        );
    }
}

fn dashmap_get_receiver(cx: &rustc_lint::LateContext<'_>, condition: &Expr<'_>) -> Option<String> {
    let ExprKind::Let(let_expression) = condition.kind else {
        return None;
    };
    let initializer = let_expression.init;
    let ExprKind::MethodCall(segment, receiver, _, _) = initializer.kind else {
        return None;
    };
    if segment.ident.name.as_str() != "get" {
        return None;
    }
    let is_dashmap = cx
        .typeck_results()
        .expr_ty(receiver)
        .peel_refs()
        .ty_adt_def()
        .is_some_and(|definition| {
            cx.tcx
                .def_path_str(definition.did())
                .ends_with("dashmap::DashMap")
        });
    is_dashmap.then(|| snippet(cx, receiver.span)).flatten()
}

fn body_can_block_or_reenter(
    cx: &rustc_lint::LateContext<'_>,
    body: &Expr<'_>,
    receiver: &str,
) -> bool {
    let Some(source) = snippet(cx, body.span) else {
        return false;
    };
    let blocks = [".await", "std::fs::", "tokio::fs::", "Command::", ".spawn("]
        .iter()
        .any(|needle| source.contains(needle));
    let reenters_same_map = ["remove", "insert", "entry"]
        .iter()
        .any(|method| source.contains(&format!("{receiver}.{method}(")));
    blocks || reenters_same_map
}

fn snippet(cx: &rustc_lint::LateContext<'_>, span: rustc_span::Span) -> Option<String> {
    cx.sess().source_map().span_to_snippet(span).ok()
}

#[cfg(test)]
mod ui_test_support {
    include!("../../ui_test_support.rs");
}

#[test]
fn ui() {
    ui_test_support::run(env!("CARGO_PKG_NAME"), env!("CARGO_MANIFEST_DIR"));
}

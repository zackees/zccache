#![feature(rustc_private)]

extern crate rustc_ast;
extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_middle;
extern crate rustc_span;

use rustc_ast::LitKind;
use rustc_errors::DiagDecorator;
use rustc_hir::def::Res;
use rustc_hir::{Expr, ExprKind};
use rustc_lint::{LateContext, LateLintPass, LintContext};
use rustc_middle::ty;
use rustc_span::{symbol::Symbol, FileName, RemapPathScopeComponents, Span};

dylint_linting::declare_late_lint! {
    /// ### What it does
    ///
    /// Bans direct `std::env::var` and `std::env::var_os` reads of the
    /// zccache-owned booleans registered in
    /// `zccache_core::config::ENVIRONMENT_VARIABLES`.
    ///
    /// ### Why is this bad?
    ///
    /// Each registered name has an intentionally strict allowlist grammar:
    /// `1` and `true` enable it; unknown values are disabled. Reading one at
    /// a call site invites a second parser or a raw presence check, which can
    /// silently invert a `*_DISABLE` or `*_NO_*` switch.
    ///
    /// ### Example
    ///
    /// ```rust,ignore
    /// let disabled = std::env::var("ZCCACHE_DISABLE").is_ok();
    /// ```
    ///
    /// Use the typed accessor instead:
    ///
    /// ```rust,ignore
    /// let disabled = zccache_core::config::zccache_disabled();
    /// ```
    pub BAN_REGISTERED_ENV_READ,
    Deny,
    "read registered zccache environment variables through their typed policy accessors"
}

const REGISTERED_NAMES: &[&str] = &[
    "ZCCACHE_DISABLE",
    "ZCCACHE_NO_SPAWN",
    "ZCCACHE_PROBE_BYPASS",
    "ZCCACHE_CACHE_TEST_BINS",
];
const POLICY_OWNER: &str = "crates/zccache-core/src/config/env_policy.rs";
const RAW_ENV_FUNCTIONS: &[&[&str]] = &[&["std", "env", "var"], &["std", "env", "var_os"]];

impl<'tcx> LateLintPass<'tcx> for BanRegisteredEnvRead {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        let ExprKind::Call(callee, arguments) = expr.kind else {
            return;
        };
        let Some(name) = registered_read_name(cx, callee, arguments) else {
            return;
        };
        if is_policy_owner(&source_filename(cx, expr.span)) {
            return;
        }
        cx.opt_span_lint(
            BAN_REGISTERED_ENV_READ,
            Some(expr.span),
            DiagDecorator(move |diag| {
                diag.primary_message(format!(
                    "`{name}` is registered environment policy; use its typed \
                     zccache_core::config accessor instead of std::env::var"
                ));
            }),
        );
    }
}

fn registered_read_name<'tcx>(
    cx: &LateContext<'tcx>,
    callee: &'tcx Expr<'tcx>,
    arguments: &'tcx [Expr<'tcx>],
) -> Option<&'static str> {
    let ExprKind::Path(qpath) = callee.kind else {
        return None;
    };
    let Res::Def(_, def_id) = cx.qpath_res(&qpath, callee.hir_id) else {
        return None;
    };
    if !RAW_ENV_FUNCTIONS
        .iter()
        .any(|path| path_ends_with(&cx.get_def_path(def_id), path))
    {
        return None;
    }
    let Some(argument) = arguments.first() else {
        return None;
    };
    match argument.kind {
        ExprKind::Lit(literal) => {
            let LitKind::Str(value, _) = literal.node else {
                return None;
            };
            REGISTERED_NAMES
                .iter()
                .copied()
                .find(|registered| *registered == value.as_str())
        }
        ExprKind::Path(qpath) => {
            let Res::Def(_, def_id) = cx.qpath_res(&qpath, argument.hir_id) else {
                return None;
            };
            registered_constant_value(cx, argument, def_id)
        }
        _ => None,
    }
}

/// Resolves a constant argument to its string value instead of trusting its
/// final path segment. This follows re-exports and local aliases while
/// avoiding false positives for unrelated constants with familiar names.
fn registered_constant_value<'tcx>(
    cx: &LateContext<'tcx>,
    argument: &'tcx Expr<'tcx>,
    def_id: rustc_hir::def_id::DefId,
) -> Option<&'static str> {
    if !matches!(
        cx.typeck_results().expr_ty(argument).peel_refs().kind(),
        ty::Str
    ) {
        return None;
    }
    let value = cx.tcx.const_eval_poly(def_id).ok()?;
    let bytes = value.try_get_slice_bytes_for_diagnostics(cx.tcx)?;
    let value = std::str::from_utf8(bytes).ok()?;
    REGISTERED_NAMES
        .iter()
        .copied()
        .find(|registered| *registered == value)
}

fn path_ends_with(path: &[Symbol], expected: &[&str]) -> bool {
    path.len() >= expected.len()
        && path[path.len() - expected.len()..]
            .iter()
            .zip(expected)
            .all(|(actual, expected)| *actual == Symbol::intern(expected))
}

fn is_policy_owner(filename: &str) -> bool {
    normalize_slashes(filename).ends_with(POLICY_OWNER)
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
mod tests {
    use super::{is_policy_owner, REGISTERED_NAMES};

    #[test]
    fn registered_names_are_unique() {
        for (index, name) in REGISTERED_NAMES.iter().enumerate() {
            assert!(
                REGISTERED_NAMES[..index]
                    .iter()
                    .all(|previous| previous != name),
                "duplicate registered environment variable: {name}",
            );
        }
    }

    #[test]
    fn policy_owner_exemption_normalizes_paths() {
        for filename in [
            "crates/zccache-core/src/config/env_policy.rs",
            "/workspace/zccache/crates/zccache-core/src/config/env_policy.rs",
            r"C:\workspace\zccache\crates\zccache-core\src\config\env_policy.rs",
        ] {
            assert!(is_policy_owner(filename), "expected owner path: {filename}");
        }
        assert!(!is_policy_owner(
            "crates/zccache-core/src/config/env_policy.rs.bak"
        ));
    }
}

#[cfg(test)]
mod ui_test_support {
    include!("../../ui_test_support.rs");
}

#[test]
fn ui() {
    ui_test_support::run(env!("CARGO_PKG_NAME"), env!("CARGO_MANIFEST_DIR"));
}

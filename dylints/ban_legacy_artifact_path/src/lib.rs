#![feature(rustc_private)]

extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_span;

use std::collections::HashSet;

use rustc_errors::DiagDecorator;
use rustc_hir::{Expr, ExprKind};
use rustc_lint::{LateContext, LateLintPass, LintContext};
use rustc_span::{FileName, RemapPathScopeComponents, Span};

dylint_linting::impl_late_lint! {
    /// Rejects code outside the artifact-layout owner that reconstructs the
    /// flat-v1 `<key>_<index>` filename convention.
    pub BAN_LEGACY_ARTIFACT_PATH,
    Deny,
    "resolve artifact payloads through the artifact-layout owner",
    BanLegacyArtifactPath::default()
}

const ALLOWLIST: &str = include_str!("allowlist.txt");

#[derive(Default)]
struct BanLegacyArtifactPath {
    /// A single `format!` expands to several HIR expressions carrying the
    /// same source callsite. Report it once rather than emitting duplicate
    /// diagnostics for expansion internals.
    reported_callsites: HashSet<Span>,
}

impl<'tcx> LateLintPass<'tcx> for BanLegacyArtifactPath {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        if !matches!(
            expr.kind,
            ExprKind::Call(..) | ExprKind::Binary(..) | ExprKind::MethodCall(..)
        ) {
            return;
        }
        self.check_snippet(cx, expr.span.source_callsite());
    }
}

impl BanLegacyArtifactPath {
    fn check_snippet(&mut self, cx: &LateContext<'_>, span: Span) {
        let filename = normalized_filename(cx, span);
        let is_ui_fixture = filename.starts_with("ui/") || filename.contains("/ui/");
        if (!filename.contains("crates/") && !is_ui_fixture) || is_allowlisted(&filename) {
            return;
        }
        let Ok(snippet) = cx.sess().source_map().span_to_snippet(span) else {
            return;
        };
        if !looks_like_legacy_artifact_path(&snippet) || !self.reported_callsites.insert(span) {
            return;
        }
        cx.opt_span_lint(
            BAN_LEGACY_ARTIFACT_PATH,
            Some(span),
            DiagDecorator(|diag| {
                diag.primary_message(
                    "flat-v1 artifact names are persistence policy; call the shared \
                     artifact-layout resolver instead of constructing `<key>_<index>`",
                );
            }),
        );
    }
}

fn looks_like_legacy_artifact_path(snippet: &str) -> bool {
    let compact: String = snippet.chars().filter(|ch| !ch.is_whitespace()).collect();
    let direct_format = compact.starts_with("format!(");
    let string_concatenation = compact.contains(".to_string()") && compact.contains('+');
    let joined_components = compact.contains(".join(\"_\")") || compact.contains(".join('_')");
    if !direct_format && !string_concatenation && !joined_components {
        return false;
    }

    let has_key = compact.contains("key");
    let has_index = compact.contains("{i}")
        || compact.contains("{idx}")
        || compact.contains("{index}")
        || compact.contains("{artifact_index}")
        || compact.contains("index")
        || compact.contains("idx")
        || compact.contains(",i)")
        || compact.contains(",idx)")
        || compact.contains(",index)")
        || (0..=9).any(|index| compact.contains(&format!("_{index}")));
    let has_separator = compact.contains("}_{")
        || compact.contains("_{}")
        || compact.contains("}_")
        || compact.contains("\"_\"")
        || compact.contains("'_'");
    has_key && has_index && has_separator
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

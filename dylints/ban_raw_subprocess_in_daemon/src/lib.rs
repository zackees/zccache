#![feature(rustc_private)]

extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_span;

use rustc_errors::DiagDecorator;
use rustc_hir::{Expr, ExprKind};
use rustc_lint::{LateContext, LateLintPass, LintContext};
use rustc_span::{symbol::Symbol, FileName, RemapPathScopeComponents};

dylint_linting::declare_late_lint! {
    /// ### What it does
    ///
    /// Bans method calls `Command::spawn`, `Command::output`, and
    /// `Command::status` on `std::process::Command` and
    /// `tokio::process::Command` in `zccache-daemon` production code.
    ///
    /// ### Why is this bad?
    ///
    /// The daemon is launched detached (no console attached). On Windows
    /// spawning a console-subsystem child from a console-less parent
    /// without `CREATE_NO_WINDOW` causes the OS to allocate a fresh
    /// console window for the child — a visible flash per cache-miss
    /// compile in the `soldr -> cargo -> rustc -> zccache-cli -> daemon
    /// -> rustc` chain.
    ///
    /// The blessed helpers in `crates/zccache-daemon/src/process.rs`
    /// (`command_output_with_priority`, `tokio_command_output_with_priority`)
    /// apply `CREATE_NO_WINDOW` along with consistent stdio piping, a
    /// Job Object attach, and child-priority adjustment. Bypassing them
    /// silently regresses one or more of those invariants.
    ///
    /// ### Known problems
    ///
    /// Test code inside `#[cfg(test)]` modules is exempted only at the
    /// file level via `src/allowlist.txt`; the lint does not yet detect
    /// `#[cfg(test)]` scope programmatically.
    ///
    /// ### Example
    ///
    /// ```rust,ignore
    /// let mut cmd = std::process::Command::new("rustc");
    /// cmd.args(["--version"]);
    /// let output = cmd.output().unwrap();
    /// ```
    ///
    /// Use instead:
    ///
    /// ```rust,ignore
    /// let mut cmd = std::process::Command::new("rustc");
    /// cmd.args(["--version"]);
    /// let output = crate::process::command_output_with_priority(
    ///     &mut cmd,
    ///     crate::process::CompilePriority::Normal,
    /// )
    /// .unwrap();
    /// ```
    pub BAN_RAW_SUBPROCESS_IN_DAEMON,
    Deny,
    "ban raw Command::{spawn, output, status} in zccache-daemon production code"
}

/// Each entry is a fully-qualified path to a banned method. Matching is
/// exact. We deliberately list `std::process::Command::*` and
/// `tokio::process::Command::*` separately — they are distinct types with
/// distinct DefIds — and intentionally omit other methods on `Command`
/// (e.g. `args`, `env`, `current_dir`) and on `Child` (e.g.
/// `wait_with_output`, `kill`). The bug class is at *spawn time*; once
/// you have a `Child`, `CREATE_NO_WINDOW` is already decided.
const BANNED_METHOD_PATHS: &[&[&str]] = &[
    &["std", "process", "Command", "spawn"],
    &["std", "process", "Command", "output"],
    &["std", "process", "Command", "status"],
    &["tokio", "process", "Command", "spawn"],
    &["tokio", "process", "Command", "output"],
    &["tokio", "process", "Command", "status"],
];

const ALLOWLIST: &str = include_str!("allowlist.txt");

/// Only daemon production code is in scope. Other crates have their own
/// spawn discipline (cli has `spawn_daemon_windows::spawn_daemon_sanitized`;
/// the ci/fingerprint crates don't spawn compilers).
const DAEMON_SOURCE_PREFIX: &str = "crates/zccache-monocrate/src/daemon/";

impl<'tcx> LateLintPass<'tcx> for BanRawSubprocessInDaemon {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        let filename = source_filename(cx, expr.span);
        let normalized = normalize_slashes(&filename);

        // Out-of-scope file → never fires.
        if !normalized.contains(DAEMON_SOURCE_PREFIX) {
            return;
        }

        // Allowlisted file → exempt by configuration.
        if is_allowlisted(&normalized) {
            return;
        }

        // Method call on a `Command`? Resolve to the canonical DefId and
        // compare to each banned path. `type_dependent_def_id` returns
        // the DefId of the method actually resolved by the trait/inherent
        // method resolver, so an alias or re-export still resolves to the
        // canonical `std::process::Command::spawn` path.
        if let ExprKind::MethodCall(_segment, _receiver, _args, _span) = expr.kind {
            if let Some(def_id) = cx.typeck_results().type_dependent_def_id(expr.hir_id) {
                for banned in BANNED_METHOD_PATHS {
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
        BAN_RAW_SUBPROCESS_IN_DAEMON,
        Some(span),
        DiagDecorator(move |diag| {
            diag.primary_message(format!(
                "`{joined}` bypasses the daemon's spawn discipline; route through \
                 `crate::process::command_output_with_priority` (sync) or \
                 `crate::process::tokio_command_output_with_priority` (async) so \
                 CREATE_NO_WINDOW, Job Object attach, and priority are applied"
            ));
        }),
    );
}

fn source_filename(cx: &LateContext<'_>, span: rustc_span::Span) -> String {
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

fn is_allowlisted(normalized: &str) -> bool {
    ALLOWLIST
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .any(|allowed| normalized.ends_with(allowed))
}

fn normalize_slashes(path: &str) -> String {
    path.replace('\\', "/")
}

fn def_path_equals(
    cx: &LateContext<'_>,
    def_id: rustc_hir::def_id::DefId,
    expected: &[&str],
) -> bool {
    let def_path = cx.get_def_path(def_id);
    if def_path.len() != expected.len() {
        return false;
    }
    def_path
        .iter()
        .zip(expected.iter())
        .all(|(actual, expected_segment)| *actual == Symbol::intern(expected_segment))
}

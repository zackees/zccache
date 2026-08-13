#![feature(rustc_private)]

extern crate rustc_ast;
extern crate rustc_errors;
extern crate rustc_span;

use std::marker::PhantomData;

use rustc_ast::token::TokenKind;
use rustc_ast::tokenstream::TokenTree;
use rustc_ast::visit::{self, Visitor};
use rustc_ast::{
    Attribute, Expr, ExprKind, Item, ItemKind, MetaItem, MetaItemInner, MetaItemKind, Path,
    UseTree, UseTreeKind,
};
use rustc_errors::DiagDecorator;
use rustc_lint::{EarlyContext, EarlyLintPass, LintContext};
use rustc_session::Session;
use rustc_span::{FileName, RemapPathScopeComponents, Span};

dylint_linting::declare_pre_expansion_lint! {
    /// ### What it does
    ///
    /// Enforces the zccache#1365 source boundary: host-platform selection and
    /// native OS APIs may only appear inside the `zccache-platform` leaf
    /// crate. Every other production source denies host cfg/cfg_attr/cfg!,
    /// native imports, and direct concrete-module references.
    ///
    /// The lint runs **pre-expansion**, so inactive host branches (Windows
    /// code compiled on Linux CI, and vice versa) are inspected too. The
    /// pre-expansion driver delivers the crate root through `check_crate`
    /// and every nested module file through the `check_attribute` /
    /// `check_item` / `check_expr` hooks; paths are scanned per item subtree
    /// because the early walker has no path delegation.
    ///
    /// ### Why is this bad?
    ///
    /// Host mechanics are selected throughout the workspace instead of at one
    /// boundary. One selector in `crates/zccache-platform/src/lib.rs` plus
    /// five neutral facades keeps that decision in one place.
    ///
    /// ### Example
    ///
    /// ```rust,ignore
    /// #[cfg(windows)]
    /// use std::os::windows::process::CommandExt;
    /// ```
    ///
    /// Use instead: move the native code behind a neutral
    /// `crate::platform::…` facade in zccache-platform.
    pub ENFORCE_PLATFORM_BOUNDARY,
    Deny,
    "confine host-platform selection and native APIs to zccache-platform"
}

/// Host predicate names that select the host platform. These are forbidden
/// outside zccache-platform even when spelled inside `any`/`all`/`not`.
const FORBIDDEN_KEYS: &[&str] = &[
    "windows",
    "unix",
    "target_os",
    "target_family",
    "target_arch",
    "target_env",
    "target_abi",
    "target_vendor",
    "target_endian",
    "target_pointer_width",
];

/// Native API roots forbidden outside zccache-platform.
const NATIVE_ROOTS: &[&str] = &["libc", "windows_sys"];

/// Concrete platform module names — private to zccache-platform.
const CONCRETE_MODULES: &[&str] = &["platform_win", "platform_linux", "platform_macos"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    AttrCfg,
    CfgMacro,
    NativeImport,
    ModuleRef,
}

/// A compiled file's position relative to the platform boundary.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// The one cfg_select! host selector in the platform crate root.
    Selector,
    /// A concrete host tree (platform_win/platform_linux/platform_macos):
    /// host cfg and native APIs live here.
    Concrete,
    /// Neutral facade files inside zccache-platform: no host cfg, no native
    /// imports, no concrete names; `platform_imp` is the only bridge.
    Facade,
    /// The lint's own fixtures: full checks.
    Ui,
    /// Ordinary production source: full checks.
    Production,
    /// Tests, benches, vendored trees, and anything outside the repo.
    OutOfScope,
}

fn classify(path: &str) -> Scope {
    // Fixtures are compiled with paths relative to the lint crate (ui/…)
    // or through the repo root (dylints/enforce_platform_boundary/ui/…).
    if path.starts_with("ui/") || path.starts_with("dylints/enforce_platform_boundary/ui/") {
        return Scope::Ui;
    }
    if path.starts_with("crates/zccache-platform/src/platform_win")
        || path.starts_with("crates/zccache-platform/src/platform_linux")
        || path.starts_with("crates/zccache-platform/src/platform_macos")
    {
        return Scope::Concrete;
    }
    if path == "crates/zccache-platform/src/lib.rs" {
        return Scope::Selector;
    }
    if path.starts_with("crates/zccache-platform/src/") {
        return Scope::Facade;
    }
    if path.starts_with("crates/") {
        // Dev-only test helpers are a non-production target.
        if path.starts_with("crates/zccache-test-support/") {
            return Scope::OutOfScope;
        }
        if path.contains("/src/test_support/") {
            return Scope::OutOfScope;
        }
        if path.contains("/tests/") || path.ends_with("_tests.rs") || path.ends_with("/tests.rs") {
            return Scope::OutOfScope;
        }
        if path.contains("/benches/") {
            return Scope::OutOfScope;
        }
        return Scope::Production;
    }
    Scope::OutOfScope
}

/// Normalizes a source filename to its repo-relative form (`crates/…` or
/// `dylints/…`) with forward slashes, or `None` when the file is outside the
/// repo (registry deps, vendored trees).
fn repo_relative_path(filename: &str) -> Option<String> {
    let normalized = filename.replace('\\', "/");
    for marker in ["crates/", "dylints/"] {
        if let Some(index) = normalized.rfind(marker) {
            return Some(normalized[index..].to_string());
        }
    }
    // UI fixtures are compiled with paths relative to the lint crate root.
    if normalized.starts_with("ui/") {
        return Some(normalized);
    }
    None
}

fn source_filename(sess: &Session, span: Span) -> Option<String> {
    match sess.source_map().span_to_filename(span) {
        FileName::Real(real) => Some(
            real.local_path()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_else(|| {
                    real.path(RemapPathScopeComponents::DIAGNOSTICS)
                        .to_string_lossy()
                        .into_owned()
                }),
        ),
        name => Some(
            name.display(RemapPathScopeComponents::DIAGNOSTICS)
                .to_string(),
        ),
    }
}

fn message(kind: Kind, normalized: &str) -> String {
    match kind {
        Kind::AttrCfg | Kind::CfgMacro => format!(
            "host-platform cfg selection ({normalized}) is not allowed here; \
             host selection belongs in crates/zccache-platform/src/lib.rs cfg_select!"
        ),
        Kind::NativeImport => format!(
            "native path ({normalized}) is not allowed here; move native code into a \
             concrete platform tree (platform_win, platform_linux, platform_macos)"
        ),
        Kind::ModuleRef => format!(
            "reference to concrete platform module ({normalized}) is not allowed here; \
             concrete modules stay private to zccache-platform"
        ),
    }
}

struct State<'ecx> {
    ecx: &'ecx EarlyContext<'ecx>,
}

/// Records one occurrence unless its source is outside the production boundary.
#[allow(clippy::too_many_arguments)]
fn record_with(
    ecx: &EarlyContext<'_>,
    span: Span,
    kind: Kind,
    normalized: &str,
) {
    let Some(path) = source_filename(ecx.sess(), span).and_then(|name| repo_relative_path(&name))
    else {
        return;
    };
    let scope = classify(&path);
    match scope {
        Scope::OutOfScope | Scope::Concrete | Scope::Selector => return,
        Scope::Facade => {
            if kind == Kind::ModuleRef && normalized == "platform_imp" {
                return;
            }
            emit_lint(ecx, span, kind, normalized);
            return;
        }
        Scope::Ui => {
            emit_lint(ecx, span, kind, normalized);
            return;
        }
        Scope::Production => {}
    }
    emit_lint(ecx, span, kind, normalized);
}

fn emit_lint(ecx: &EarlyContext<'_>, span: Span, kind: Kind, normalized: &str) {
    // Keep the substring "ui" out of these messages: compiletest
    // normalizes fixture paths and rewrites "ui" to `$DIR` mid-word.
    let text = message(kind, normalized);
    ecx.opt_span_lint(
        ENFORCE_PLATFORM_BOUNDARY,
        Some(span),
        DiagDecorator(move |diag| {
            diag.primary_message(text);
        }),
    );
}

impl<'ecx> State<'ecx> {
    /// Records one occurrence unless its source location is allowed.
    fn record(&mut self, span: Span, kind: Kind, normalized: &str) {
        record_with(
            self.ecx,
            span,
            kind,
            normalized,
        )
    }

    fn check_attribute(&mut self, attr: &Attribute) {
        let Some(meta) = attr.meta() else {
            return;
        };
        self.check_meta(&meta);
    }

    /// Walks a MetaItem tree and flags every leaf whose path selects the host
    /// platform. Host-independent leaves (`test`, `feature = "…"`,
    /// `debug_assertions`, …) are allowed everywhere.
    fn check_meta(&mut self, meta: &MetaItem) {
        let first = meta.path.segments.first();
        match &meta.kind {
            MetaItemKind::Word | MetaItemKind::NameValue(_) => {
                if let Some(segment) = first {
                    let name = segment.ident.name.as_str();
                    if FORBIDDEN_KEYS.contains(&name) {
                        self.record(meta.span, Kind::AttrCfg, name);
                    }
                }
            }
            MetaItemKind::List(items) => {
                for item in items {
                    if let MetaItemInner::MetaItem(nested) = item {
                        self.check_meta(nested);
                    }
                }
            }
        }
    }

    /// Flags `cfg!(…)` invocations whose tokens name a host predicate.
    /// Ident tokens are matched (not string literals), so
    /// `cfg!(feature = "windows")` stays legal.
    fn check_cfg_macro(&mut self, expr: &Expr) {
        let ExprKind::MacCall(mac) = &expr.kind else {
            return;
        };
        let Some(first) = mac.path.segments.first() else {
            return;
        };
        if first.ident.name.as_str() != "cfg" {
            return;
        }
        self.check_tokens(expr.span, &mac.args.tokens);
    }

    fn check_tokens(&mut self, span: Span, tokens: &rustc_ast::tokenstream::TokenStream) {
        for tree in tokens.iter() {
            match tree {
                TokenTree::Token(token, _) => {
                    if let TokenKind::Ident(name, _) = token.kind {
                        let name = name.as_str();
                        if FORBIDDEN_KEYS.contains(&name) {
                            self.record(span, Kind::CfgMacro, name);
                        }
                    }
                }
                TokenTree::Delimited(_, _, _, inner) => self.check_tokens(span, inner),
            }
        }
    }

    /// Flags native roots, `std::os::{windows,unix}`, and concrete module
    /// names anywhere a path appears in the pre-expansion source.
    fn check_path(&mut self, path: &Path) {
        let segments: Vec<&str> = path
            .segments
            .iter()
            .map(|segment| segment.ident.name.as_str())
            .collect();
        if segments.len() >= 3
            && segments[0] == "std"
            && segments[1] == "os"
            && (segments[2] == "windows" || segments[2] == "unix")
        {
            self.record(
                path.span,
                Kind::NativeImport,
                &format!("std::os::{}", segments[2]),
            );
            return;
        }
        if let Some(first) = segments.first() {
            if NATIVE_ROOTS.contains(first) {
                self.record(path.span, Kind::NativeImport, first);
                return;
            }
        }
        for segment in &segments {
            if CONCRETE_MODULES.contains(segment) {
                self.record(path.span, Kind::ModuleRef, segment);
                return;
            }
        }
        if segments.contains(&"platform_imp") {
            self.record(path.span, Kind::ModuleRef, "platform_imp");
        }
    }

    fn check_use_tree(&mut self, tree: &UseTree) {
        self.check_path(&tree.prefix);
        match &tree.kind {
            UseTreeKind::Simple(_) | UseTreeKind::Glob(_) => {}
            UseTreeKind::Nested { items, .. } => {
                for (nested, _id) in items {
                    self.check_use_tree(nested);
                }
            }
        }
    }

    fn check_item(&mut self, item: &Item) {
        if let ItemKind::ExternCrate(orig, ident) = &item.kind {
            for name in [
                orig.as_ref().map(|symbol| symbol.as_str()),
                Some(ident.name.as_str()),
            ] {
                if let Some(name) = name {
                    if NATIVE_ROOTS.contains(&name) {
                        self.record(item.span, Kind::NativeImport, name);
                    }
                }
            }
        }
        if let ItemKind::Use(tree) = &item.kind {
            self.check_use_tree(tree);
        }
        // Paths are not delegated by the walker, so scan this item's own
        // subtree here. Nested items are skipped: the walker delivers them
        // through check_item separately, so every path is visited once.
        let mut scanner = PathScanner {
            ecx: self.ecx,
            marker: PhantomData,
        };
        visit::walk_item(&mut scanner, item);
    }
}

/// Scans an item's subtree for paths without descending into nested items
/// (which arrive through their own `check_item` call).
struct PathScanner<'a, 'ast> {
    ecx: &'a EarlyContext<'a>,
    marker: PhantomData<&'ast ()>,
}

impl<'a, 'ast> Visitor<'ast> for PathScanner<'a, 'ast> {
    fn visit_item(&mut self, _item: &'ast Item) {}

    fn visit_path(&mut self, path: &'ast Path) {
        State { ecx: self.ecx }.check_path(path);
    }
}

/// Runs `f` with source-classifying state for one lint callback.
fn with_state<'ecx>(cx: &'ecx EarlyContext<'ecx>, f: impl FnOnce(&mut State<'ecx>)) {
    let mut state = State { ecx: cx };
    f(&mut state);
}

impl EarlyLintPass for EnforcePlatformBoundary {
    fn check_attribute(&mut self, cx: &EarlyContext<'_>, attr: &Attribute) {
        with_state(cx, |state| state.check_attribute(attr));
    }

    fn check_item(&mut self, cx: &EarlyContext<'_>, item: &Item) {
        with_state(cx, |state| state.check_item(item));
    }

    fn check_expr(&mut self, cx: &EarlyContext<'_>, expr: &Expr) {
        with_state(cx, |state| state.check_cfg_macro(expr));
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

//! Request-scoped C/C++ dependency discovery policy.

use super::*;

const FAST_ENV: &str = "ZCCACHE_FAST";
const SCAN_SYSTEM_HEADERS_ENV: &str = "ZCCACHE_SCAN_SYSTEM_HEADERS";

/// How compiler-selected C/C++ dependencies are recorded on a cache miss.
///
/// Both variants use the depgraph as a direct-mode manifest on later requests.
/// They differ only in whether the compiler includes system headers in the
/// first manifest (`-MD`) or omits them (`-MMD`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DependencyDiscoveryMode {
    AllHeaders,
    SkipSystemHeaders,
}

impl DependencyDiscoveryMode {
    pub(super) fn from_client_env(client_env: Option<&[(String, String)]>) -> Result<Self, String> {
        if let Some(value) = client_env_value(client_env, SCAN_SYSTEM_HEADERS_ENV) {
            return parse_bool(SCAN_SYSTEM_HEADERS_ENV, value).map(|scan| {
                if scan {
                    Self::AllHeaders
                } else {
                    Self::SkipSystemHeaders
                }
            });
        }

        let fast = client_env_value(client_env, FAST_ENV)
            .map(|value| parse_bool(FAST_ENV, value))
            .transpose()?
            .unwrap_or(false);
        Ok(if fast {
            Self::SkipSystemHeaders
        } else {
            Self::AllHeaders
        })
    }

    pub(super) fn injected_depfile_flag(self) -> &'static str {
        match self {
            Self::AllHeaders => "-MD",
            Self::SkipSystemHeaders => "-MMD",
        }
    }

    pub(super) fn use_mmd(self) -> bool {
        matches!(self, Self::SkipSystemHeaders)
    }

    /// Salt C/C++ context keys so a partial fast-mode manifest can never be
    /// reused after the caller switches back to the correctness-first mode.
    pub(super) fn apply_to_cc_context(self, ctx: &mut CompileContext, dep_flags: &UserDepFlags) {
        let marker = match self {
            Self::AllHeaders => "zccache:dependency-discovery=all-headers",
            Self::SkipSystemHeaders => "zccache:dependency-discovery=skip-system-headers",
        };
        if !ctx.unknown_flags.iter().any(|flag| flag == marker) {
            ctx.unknown_flags.push(marker.to_string());
        }
        let depfile_marker = if dep_flags.has_mmd {
            "zccache:user-depfile=mmd"
        } else if dep_flags.has_md {
            "zccache:user-depfile=md"
        } else {
            "zccache:user-depfile=none"
        };
        if !ctx.unknown_flags.iter().any(|flag| flag == depfile_marker) {
            ctx.unknown_flags.push(depfile_marker.to_string());
        }
    }

    pub(super) fn apply_user_depfile_content_to_cc_context(
        ctx: &mut CompileContext,
        dep_flags: &UserDepFlags,
        args: &[String],
    ) {
        if !dep_flags.has_md {
            return;
        }
        let mut index = 0;
        let mut content_index = 0;
        while index < args.len() {
            let arg = &args[index];
            if arg == "-MF" {
                if args.get(index + 1).is_some_and(|value| value == "-") {
                    ctx.unknown_flags
                        .push("zccache:user-depfile-output=stdout".to_string());
                    content_index += 1;
                }
                index += 2;
                continue;
            }
            if arg == "-MF-" {
                ctx.unknown_flags
                    .push("zccache:user-depfile-output=stdout".to_string());
                content_index += 1;
            } else if !arg.starts_with("-MF") {
                ctx.unknown_flags.push(format!(
                    "zccache:user-depfile-argv={content_index}:{}:{arg}",
                    arg.len()
                ));
                content_index += 1;
            }
            index += 1;
        }
    }

    /// Apply the same user-vs-system root precedence used by the compiler's
    /// include search when a static preflight must approximate `-MMD`.
    pub(super) fn retain_tracked_headers(
        self,
        headers: &mut Vec<NormalizedPath>,
        include_search: &crate::depgraph::IncludeSearchPaths,
    ) {
        headers.retain(|path| self.tracks_header(path, include_search));
    }

    pub(super) fn tracks_header(
        self,
        path: &NormalizedPath,
        include_search: &crate::depgraph::IncludeSearchPaths,
    ) -> bool {
        if self == Self::AllHeaders {
            return true;
        }
        let selected_by_user_root = include_search
            .iquote
            .iter()
            .chain(&include_search.user)
            .any(|root| path.starts_with(root.as_path()));
        selected_by_user_root
            || !include_search
                .system
                .iter()
                .chain(&include_search.after)
                .any(|root| path.starts_with(root.as_path()))
    }

    /// Approximate compiler provenance before a private `-MMD` manifest exists.
    /// A quoted path resolved relative to the including file remains a user
    /// dependency even when both files happen to live beneath a broad SDK root.
    #[allow(clippy::cmp_owned)] // Normalization is required before comparing a quoted relative path.
    pub(super) fn tracks_static_include(
        self,
        including_file: &Path,
        directive: &crate::depgraph::scanner::IncludeDirective,
        path: &NormalizedPath,
        include_search: &crate::depgraph::IncludeSearchPaths,
    ) -> bool {
        if matches!(
            directive.kind,
            crate::depgraph::scanner::IncludeKind::Quoted
        ) && including_file
            .parent()
            .is_some_and(|parent| NormalizedPath::from(parent.join(&directive.path)) == *path)
        {
            return true;
        }
        self.tracks_header(path, include_search)
    }

    /// Make a scanner fallback conservative when compiler provenance is lost.
    pub(super) fn apply_static_fallback(
        self,
        result: &mut crate::depgraph::ScanResult,
        include_search: &crate::depgraph::IncludeSearchPaths,
    ) {
        if self == Self::SkipSystemHeaders {
            self.retain_tracked_headers(&mut result.resolved, include_search);
            result.has_computed = true;
        } else {
            result.has_computed |= !result.unresolved.is_empty();
        }
    }
}

fn parse_bool(name: &str, value: &str) -> Result<bool, String> {
    let value = value.trim();
    if value == "1"
        || value.eq_ignore_ascii_case("true")
        || value.eq_ignore_ascii_case("yes")
        || value.eq_ignore_ascii_case("on")
    {
        Ok(true)
    } else if value == "0"
        || value.eq_ignore_ascii_case("false")
        || value.eq_ignore_ascii_case("no")
        || value.eq_ignore_ascii_case("off")
    {
        Ok(false)
    } else {
        Err(format!(
            "{name} must be one of 1/0, true/false, yes/no, or on/off (got {value:?})"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(values: &[(&str, &str)]) -> Vec<(String, String)> {
        values
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn safe_mode_is_default() {
        assert_eq!(
            DependencyDiscoveryMode::from_client_env(None).unwrap(),
            DependencyDiscoveryMode::AllHeaders
        );
    }

    #[test]
    fn fast_preset_skips_system_headers() {
        let values = env(&[(FAST_ENV, "1")]);
        assert_eq!(
            DependencyDiscoveryMode::from_client_env(Some(&values)).unwrap(),
            DependencyDiscoveryMode::SkipSystemHeaders
        );
    }

    #[test]
    fn fine_grained_setting_overrides_fast() {
        let values = env(&[(FAST_ENV, "1"), (SCAN_SYSTEM_HEADERS_ENV, " TRUE ")]);
        assert_eq!(
            DependencyDiscoveryMode::from_client_env(Some(&values)).unwrap(),
            DependencyDiscoveryMode::AllHeaders
        );
    }

    #[test]
    fn user_root_wins_when_nested_beneath_system_root() {
        let search = crate::depgraph::IncludeSearchPaths {
            user: vec!["/sdk/vendor".into()],
            system: vec!["/sdk".into()],
            ..Default::default()
        };
        let user_header: NormalizedPath = "/sdk/vendor/api.hpp".into();
        let system_header: NormalizedPath = "/sdk/stdlib/vector".into();
        let mut headers = vec![user_header.clone(), system_header.clone()];

        DependencyDiscoveryMode::SkipSystemHeaders.retain_tracked_headers(&mut headers, &search);

        assert_eq!(headers, vec![user_header]);
    }

    #[test]
    fn quoted_sibling_remains_user_input_beneath_system_root() {
        let search = crate::depgraph::IncludeSearchPaths {
            system: vec!["/sdk".into()],
            ..Default::default()
        };
        let directive = crate::depgraph::scanner::IncludeDirective {
            kind: crate::depgraph::scanner::IncludeKind::Quoted,
            path: "config.hpp".into(),
            line: 1,
        };

        assert!(
            DependencyDiscoveryMode::SkipSystemHeaders.tracks_static_include(
                Path::new("/sdk/project/main.cpp"),
                &directive,
                &NormalizedPath::from("/sdk/project/config.hpp"),
                &search,
            )
        );
    }

    #[test]
    fn unresolved_safe_mode_fallback_disables_direct_hits() {
        let mut result = crate::depgraph::ScanResult {
            resolved: Vec::new(),
            unresolved: vec!["compiler-resolved-only.h".into()],
            has_computed: false,
        };

        DependencyDiscoveryMode::AllHeaders.apply_static_fallback(&mut result, &Default::default());

        assert!(result.has_computed);
    }

    #[test]
    fn user_depfile_modes_salt_context_keys() {
        let context_for = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
            let parsed = crate::depgraph::args::parse_gnu_args(&args, Path::new("/work"));
            let dep_flags = parsed.dep_flags.clone();
            let mut context =
                CompileContext::from_parsed_args(parsed, crate::hash::hash_bytes(b"test-fixture"));
            DependencyDiscoveryMode::AllHeaders.apply_to_cc_context(&mut context, &dep_flags);
            context.context_key()
        };

        let none = context_for(&["-c", "main.c"]);
        let md = context_for(&["-c", "main.c", "-MD"]);
        let mmd = context_for(&["-c", "main.c", "-MMD"]);

        assert_ne!(none, md);
        assert_ne!(md, mmd);
        assert_ne!(none, mmd);
    }

    #[test]
    fn depfile_content_shape_salts_context_keys() {
        let context_for = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
            let parsed = crate::depgraph::args::parse_gnu_args(&args, Path::new("/work"));
            let dep_flags = parsed.dep_flags.clone();
            let mut context =
                CompileContext::from_parsed_args(parsed, crate::hash::hash_bytes(b"test-fixture"));
            DependencyDiscoveryMode::AllHeaders.apply_to_cc_context(&mut context, &dep_flags);
            DependencyDiscoveryMode::apply_user_depfile_content_to_cc_context(
                &mut context,
                &dep_flags,
                &args,
            );
            context.context_key()
        };

        let output_a = context_for(&["-c", "main.c", "-o", "a.o", "-MD"]);
        let output_b = context_for(&["-c", "main.c", "-o", "/work/a.o", "-MD"]);
        let target = context_for(&[
            "-c",
            "main.c",
            "-o",
            "a.o",
            "-MD",
            "-MT",
            "explicit.o",
            "-MP",
        ]);
        let moved_depfile = context_for(&["-c", "main.c", "-o", "a.o", "-MD", "-MF", "other.d"]);
        let stdout_depfile = context_for(&["-c", "main.c", "-o", "a.o", "-MD", "-MF", "-"]);

        assert_ne!(output_a, output_b);
        assert_ne!(output_a, target);
        assert_eq!(output_a, moved_depfile);
        assert_ne!(output_a, stdout_depfile);
    }
}

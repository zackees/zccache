//! Policy for degenerate uncached fallbacks (issue #1211).
//!
//! When the daemon/cache pipeline fails before dispatch, the wrapper
//! historically ran the tool directly, uncached. On CI that hides real
//! infrastructure failures behind slow-but-green builds, so the policy is:
//! hard error on CI, yellow warning everywhere else — always carrying the
//! concrete daemon-failure reason.

/// Explicit override for the fallback policy: `error` or `warn`.
/// Wins over CI auto-detection.
pub(super) const FALLBACK_POLICY_ENV: &str = "ZCCACHE_FALLBACK";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FallbackPolicy {
    /// Refuse the uncached fallback and fail the compile (CI default).
    Error,
    /// Warn (yellow) and run the tool uncached (interactive default).
    Warn,
}

/// A resolved policy plus the human-readable source of the decision, so
/// the refusal/warning message can say *why* this policy applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResolvedFallbackPolicy {
    pub(super) policy: FallbackPolicy,
    pub(super) source: String,
}

pub(super) fn resolve_fallback_policy() -> ResolvedFallbackPolicy {
    resolve_fallback_policy_with_env(|name| std::env::var(name).ok())
}

/// Testable variant taking an env lookup closure. Resolution order:
/// explicit `ZCCACHE_FALLBACK=error|warn` → default `Error`.
///
/// The default is `Error` on every host, not just CI (owner directive,
/// 2026-07-24): cached artifacts are materialized as READ-ONLY hardlinks
/// (COW-lite, #1038/#1039), so a direct uncached compiler run cannot
/// overwrite them anyway — the fallback doesn't merely lose the cache,
/// it fails or corrupts the build. An unreachable daemon is therefore a
/// hard error; `ZCCACHE_FALLBACK=warn` is the explicit escape hatch for
/// build trees that never received hardlinked artifacts. An unrecognized
/// override value is reported loudly and treated as unset.
pub(super) fn resolve_fallback_policy_with_env<F>(lookup: F) -> ResolvedFallbackPolicy
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(value) = lookup(FALLBACK_POLICY_ENV) {
        match value.trim().to_ascii_lowercase().as_str() {
            "error" => {
                return ResolvedFallbackPolicy {
                    policy: FallbackPolicy::Error,
                    source: format!("{FALLBACK_POLICY_ENV}=error"),
                }
            }
            "warn" => {
                return ResolvedFallbackPolicy {
                    policy: FallbackPolicy::Warn,
                    source: format!("{FALLBACK_POLICY_ENV}=warn"),
                }
            }
            other => {
                eprintln!(
                    "zccache[warn][F]: unrecognized {FALLBACK_POLICY_ENV}={other:?} \
                     (expected \"error\" or \"warn\"); using the default policy"
                );
            }
        }
    }
    ResolvedFallbackPolicy {
        policy: FallbackPolicy::Error,
        source: format!(
            "default: uncached fallback is unsafe with read-only hardlinked \
             artifacts; set {FALLBACK_POLICY_ENV}=warn to allow it"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    #[test]
    fn explicit_error_override_wins() {
        let resolved = resolve_fallback_policy_with_env(env(&[("ZCCACHE_FALLBACK", "error")]));
        assert_eq!(resolved.policy, FallbackPolicy::Error);
        assert_eq!(resolved.source, "ZCCACHE_FALLBACK=error");
    }

    #[test]
    fn explicit_warn_override_allows_fallback() {
        let resolved = resolve_fallback_policy_with_env(env(&[("ZCCACHE_FALLBACK", "warn")]));
        assert_eq!(resolved.policy, FallbackPolicy::Warn);
        assert_eq!(resolved.source, "ZCCACHE_FALLBACK=warn");
    }

    #[test]
    fn default_is_hard_error_on_every_host() {
        let resolved = resolve_fallback_policy_with_env(env(&[]));
        assert_eq!(resolved.policy, FallbackPolicy::Error);
        assert!(
            resolved.source.contains("read-only hardlinked"),
            "refusal source must explain why the fallback is unsafe: {}",
            resolved.source,
        );
    }

    #[test]
    fn unrecognized_override_falls_through_to_error_default() {
        let resolved = resolve_fallback_policy_with_env(env(&[("ZCCACHE_FALLBACK", "banana")]));
        assert_eq!(resolved.policy, FallbackPolicy::Error);
    }
}

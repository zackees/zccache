//! Request-scoped admission policy for rustc `--test` harness links
//! (zccache#1525, zccache#1550).
//!
//! A `--test` harness statically links its whole dependency graph, so it is
//! large and changes with any transitive source edit. zccache#1525 therefore
//! refuses it at cache admission by default. Some workloads do re-request an
//! identical harness — a CI re-run after a flake, or a docs/workflow-only
//! change — and those callers may opt in with [`TestHarnessAdmission::All`].
//!
//! The policy is resolved **per compile request**, in this order:
//!
//! 1. an explicit option on the embedded API
//!    (`zccache_daemon_core::embedded::CompileOptions`);
//! 2. [`CACHE_TEST_BINS_ENV`] in the request's forwarded client environment,
//!    when the client set it at all (`1`/`true` admits, any other value
//!    refuses);
//! 3. the service process's own [`CACHE_TEST_BINS_ENV`] — the pre-#1550
//!    behaviour, unchanged for every caller that supplies neither.
//!
//! Resolution never mutates the process environment or any service state, so
//! one long-running service can serve alternating policies.

use super::env_policy::{cache_test_binaries_enabled, owned_flag_enabled, CACHE_TEST_BINS_ENV};

/// Largest admitted harness, in logical compiler-artifact bytes.
///
/// An admitted harness whose planned outputs exceed this is executed and
/// materialized normally but not stored. The bound keeps one runaway link
/// product from displacing the shared library units the store exists for;
/// the store's own byte budget and age expiry bound the total. Measured
/// debug harnesses: zccache's largest is 141 MB, clud's 203 MB.
pub const MAX_ADMITTED_TEST_HARNESS_BYTES: u64 = 512 * 1024 * 1024;

/// Whether rustc `--test` harness link products may enter the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TestHarnessAdmission {
    /// Store shared units only; a `--test` harness runs uncached. Default.
    SharedOnly,
    /// Also store `--test` harnesses, keyed exactly and size-bounded by
    /// [`MAX_ADMITTED_TEST_HARNESS_BYTES`].
    All,
}

impl TestHarnessAdmission {
    /// True when a `--test` harness may be stored.
    #[must_use]
    pub const fn admits_test_harness(self) -> bool {
        matches!(self, Self::All)
    }

    /// Parse one [`CACHE_TEST_BINS_ENV`] value with the canonical owned
    /// boolean grammar: `1` or case-insensitive `true` admits, anything else
    /// refuses.
    #[must_use]
    pub fn from_env_value(value: &str) -> Self {
        if owned_flag_enabled(Some(value)) {
            Self::All
        } else {
            Self::SharedOnly
        }
    }

    /// The service process's default: [`CACHE_TEST_BINS_ENV`] in its own
    /// environment.
    #[must_use]
    pub fn from_process_env() -> Self {
        if cache_test_binaries_enabled() {
            Self::All
        } else {
            Self::SharedOnly
        }
    }

    /// The policy a forwarded client environment asks for, or `None` when the
    /// client did not set [`CACHE_TEST_BINS_ENV`]. The last assignment wins,
    /// as it would in a process environment.
    #[must_use]
    pub fn from_client_env(client_env: &[(String, String)]) -> Option<Self> {
        client_env
            .iter()
            .rev()
            .find(|(name, _)| name == CACHE_TEST_BINS_ENV)
            .map(|(_, value)| Self::from_env_value(value))
    }

    /// Resolve one request's policy: explicit option, then the request's
    /// forwarded environment, then the service process default.
    #[must_use]
    pub fn resolve(explicit: Option<Self>, client_env: Option<&[(String, String)]>) -> Self {
        explicit
            .or_else(|| client_env.and_then(Self::from_client_env))
            .unwrap_or_else(Self::from_process_env)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn env_value_uses_the_owned_boolean_grammar() {
        for admitted in ["1", "true", "TRUE", " true "] {
            assert_eq!(
                TestHarnessAdmission::from_env_value(admitted),
                TestHarnessAdmission::All
            );
        }
        for refused in ["0", "false", "yes", "on", ""] {
            assert_eq!(
                TestHarnessAdmission::from_env_value(refused),
                TestHarnessAdmission::SharedOnly
            );
        }
    }

    #[test]
    fn client_env_is_absent_unless_the_client_set_the_variable() {
        assert_eq!(
            TestHarnessAdmission::from_client_env(&env(&[("CARGO", "x")])),
            None
        );
        assert_eq!(
            TestHarnessAdmission::from_client_env(&env(&[(CACHE_TEST_BINS_ENV, "1")])),
            Some(TestHarnessAdmission::All)
        );
        assert_eq!(
            TestHarnessAdmission::from_client_env(&env(&[(CACHE_TEST_BINS_ENV, "0")])),
            Some(TestHarnessAdmission::SharedOnly)
        );
        assert_eq!(
            TestHarnessAdmission::from_client_env(&env(&[
                (CACHE_TEST_BINS_ENV, "1"),
                (CACHE_TEST_BINS_ENV, "0"),
            ])),
            Some(TestHarnessAdmission::SharedOnly)
        );
    }

    #[test]
    fn explicit_option_overrides_the_request_environment() {
        let admit = env(&[(CACHE_TEST_BINS_ENV, "1")]);
        let refuse = env(&[(CACHE_TEST_BINS_ENV, "0")]);
        assert_eq!(
            TestHarnessAdmission::resolve(Some(TestHarnessAdmission::SharedOnly), Some(&admit)),
            TestHarnessAdmission::SharedOnly
        );
        assert_eq!(
            TestHarnessAdmission::resolve(Some(TestHarnessAdmission::All), Some(&refuse)),
            TestHarnessAdmission::All
        );
        assert_eq!(
            TestHarnessAdmission::resolve(None, Some(&admit)),
            TestHarnessAdmission::All
        );
        assert_eq!(
            TestHarnessAdmission::resolve(None, Some(&refuse)),
            TestHarnessAdmission::SharedOnly
        );
    }
}

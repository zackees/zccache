//! Registered environment-variable policy for zccache-owned boolean switches.
//!
//! This deliberately covers only the coherent `{1, true}` family. Foreign
//! variables retain their denylist semantics, parsing errors remain errors,
//! and diagnostic presence switches remain presence based; those are separate
//! policies and must not be normalized accidentally.

use std::ffi::OsStr;

/// Value policy for a registered environment variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvironmentVariableKind {
    /// A zccache-owned boolean: only `1` or case-insensitive `true` enables.
    OwnedBoolean,
}

/// One registered zccache environment variable and its documented policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvironmentVariableDeclaration {
    /// Stable environment-variable name.
    pub name: &'static str,
    /// How this variable's value is interpreted.
    pub kind: EnvironmentVariableKind,
    /// Concise operator-facing purpose.
    pub help: &'static str,
}

/// Set this to bypass zccache completely for one process invocation.
const ZCCACHE_DISABLE_ENV: &str = "ZCCACHE_DISABLE";
/// Set this in an embedding host to forbid standalone daemon launches.
pub const NO_SPAWN_ENV: &str = "ZCCACHE_NO_SPAWN";
/// Set this to run cheap compiler probes without an IPC round trip.
const ZCCACHE_PROBE_BYPASS_ENV: &str = "ZCCACHE_PROBE_BYPASS";
/// Set this to allow caching Rust `--test` harness links.
pub const CACHE_TEST_BINS_ENV: &str = "ZCCACHE_CACHE_TEST_BINS";

/// The complete registry for the owned-boolean policy family.
///
/// New entries require a typed accessor below. The `ban_registered_env_read`
/// Dylint rejects direct `std::env::{var,var_os}` reads of these names outside
/// this owner, preserving one parser and one lookup point per policy.
pub const ENVIRONMENT_VARIABLES: &[EnvironmentVariableDeclaration] = &[
    EnvironmentVariableDeclaration {
        name: ZCCACHE_DISABLE_ENV,
        kind: EnvironmentVariableKind::OwnedBoolean,
        help: "Bypass zccache and run the compiler directly.",
    },
    EnvironmentVariableDeclaration {
        name: NO_SPAWN_ENV,
        kind: EnvironmentVariableKind::OwnedBoolean,
        help: "Forbid standalone zccache daemon launches from an embedding host.",
    },
    EnvironmentVariableDeclaration {
        name: ZCCACHE_PROBE_BYPASS_ENV,
        kind: EnvironmentVariableKind::OwnedBoolean,
        help: "Run cheap compiler probes without using the cache daemon.",
    },
    EnvironmentVariableDeclaration {
        name: CACHE_TEST_BINS_ENV,
        kind: EnvironmentVariableKind::OwnedBoolean,
        help: "Opt into caching Rust test-harness links.",
    },
];

#[derive(Debug, Clone, Copy)]
enum OwnedBoolean {
    Disable,
    NoSpawn,
    ProbeBypass,
    CacheTestBinaries,
}

impl OwnedBoolean {
    const fn name(self) -> &'static str {
        match self {
            Self::Disable => ZCCACHE_DISABLE_ENV,
            Self::NoSpawn => NO_SPAWN_ENV,
            Self::ProbeBypass => ZCCACHE_PROBE_BYPASS_ENV,
            Self::CacheTestBinaries => CACHE_TEST_BINS_ENV,
        }
    }

    fn enabled(self) -> bool {
        if matches!(self, Self::NoSpawn) {
            return no_spawn_from_env_value(std::env::var_os(self.name()).as_deref());
        }
        owned_flag_enabled(std::env::var(self.name()).ok().as_deref())
    }
}

/// The canonical grammar for a zccache-owned boolean switch: `1` or
/// case-insensitive `true` enables it; every other value, including an
/// unrecognised one, is disabled. Whitespace around a valid value is ignored.
#[must_use]
pub fn owned_flag_enabled(value: Option<&str>) -> bool {
    value.is_some_and(|raw| {
        let trimmed = raw.trim();
        trimmed == "1" || trimmed.eq_ignore_ascii_case("true")
    })
}

/// Applies [`owned_flag_enabled`] to an environment variable by name.
///
/// This compatibility shim preserves the former public
/// `zccache::core::config::owned_env_flag_enabled` API. New internal callers
/// must use the typed accessor for their registered setting so the registry
/// remains the sole owner of lookup and parsing policy.
#[deprecated(
    note = "use the typed zccache_core::config accessor for the registered environment variable"
)]
#[must_use]
pub fn owned_env_flag_enabled(name: &str) -> bool {
    owned_flag_enabled(std::env::var(name).ok().as_deref())
}

/// True when `ZCCACHE_DISABLE` bypasses the cache for this process.
#[must_use]
pub fn zccache_disabled() -> bool {
    OwnedBoolean::Disable.enabled()
}

/// True when the host forbids standalone daemon spawns via [`NO_SPAWN_ENV`].
#[must_use]
pub fn daemon_spawn_disabled() -> bool {
    OwnedBoolean::NoSpawn.enabled()
}

/// True when `ZCCACHE_PROBE_BYPASS` opts cheap probes out of the daemon path.
#[must_use]
pub fn probe_bypass_enabled() -> bool {
    OwnedBoolean::ProbeBypass.enabled()
}

/// True when `ZCCACHE_CACHE_TEST_BINS` re-admits Rust `--test` harness links.
#[must_use]
pub fn cache_test_binaries_enabled() -> bool {
    OwnedBoolean::CacheTestBinaries.enabled()
}

/// Testable core of [`daemon_spawn_disabled`] — no environment access.
#[must_use]
pub(crate) fn no_spawn_from_env_value(value: Option<&OsStr>) -> bool {
    owned_flag_enabled(value.map(|value| value.to_string_lossy()).as_deref())
}

/// Standard error for a refused spawn. Names [`NO_SPAWN_ENV`] so operators
/// can find the knob, and points at the embedded service so the failure is
/// self-explaining in host contexts.
#[must_use]
pub fn no_spawn_error(daemon_name: &str) -> String {
    format!(
        "{daemon_name} spawn disabled by host ({NO_SPAWN_ENV}=1); \
         this host serves compiles through an embedded zccache service"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_contains_only_owned_boolean_switches() {
        assert_eq!(ENVIRONMENT_VARIABLES.len(), 4);
        assert!(ENVIRONMENT_VARIABLES
            .iter()
            .all(|declaration| { declaration.kind == EnvironmentVariableKind::OwnedBoolean }));
    }

    #[test]
    fn registry_names_are_unique() {
        for (index, declaration) in ENVIRONMENT_VARIABLES.iter().enumerate() {
            assert!(
                ENVIRONMENT_VARIABLES[..index]
                    .iter()
                    .all(|previous| previous.name != declaration.name),
                "duplicate registered environment variable: {}",
                declaration.name,
            );
        }
    }

    #[test]
    #[allow(deprecated)]
    fn compatibility_accessor_keeps_the_former_dynamic_lookup() {
        const COMPATIBILITY_TEST_ENV: &str = "ZCCACHE_ISSUE_1478_COMPATIBILITY_TEST";

        std::env::remove_var(COMPATIBILITY_TEST_ENV);
        assert!(!owned_env_flag_enabled(COMPATIBILITY_TEST_ENV));

        std::env::set_var(COMPATIBILITY_TEST_ENV, " true ");
        assert!(owned_env_flag_enabled(COMPATIBILITY_TEST_ENV));

        std::env::remove_var(COMPATIBILITY_TEST_ENV);
    }
}

//! Daemon namespace + IPC component sanitization helpers.
//!
//! Owns `ZCCACHE_DAEMON_NAMESPACE` parsing, `sanitize_*` helpers used by
//! IPC endpoint and path naming, and a small FNV-1a hasher used for stable
//! short identifiers (also reused by `resolve.rs` for colocation paths).

use super::{DAEMON_NAMESPACE_ENV, DEFAULT_DAEMON_NAMESPACE};
use std::ffi::OsString;
use std::path::Path;

/// Returns the active daemon/socket namespace, if explicitly configured.
///
/// Values are trimmed and normalized to a path/pipe-safe ASCII component:
/// alphanumerics plus `-`, `_`, and `.` are preserved; every other character
/// becomes `_`. Long values retain a readable prefix plus an 8-hex hash to
/// avoid collisions.
#[must_use]
pub fn daemon_namespace() -> Option<String> {
    daemon_namespace_from_env_value(std::env::var_os(DAEMON_NAMESPACE_ENV))
}

/// Returns the namespace label to show in diagnostics and status JSON.
#[must_use]
pub fn daemon_namespace_label() -> String {
    daemon_namespace().unwrap_or_else(|| DEFAULT_DAEMON_NAMESPACE.to_string())
}

pub(super) fn daemon_namespace_from_env_value(value: Option<OsString>) -> Option<String> {
    let value = value?;
    if value.is_empty() {
        return None;
    }
    sanitize_daemon_namespace(&value.to_string_lossy())
}

pub fn sanitize_daemon_namespace(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let sanitized: String = trimmed
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.len() <= 32 {
        return Some(sanitized);
    }
    let prefix: String = sanitized.chars().take(32).collect();
    Some(format!("{prefix}-{}", namespace_short_hash(trimmed)))
}

fn namespace_short_hash(value: &str) -> String {
    fnv_short_hash(value.as_bytes())
}

/// Sanitize a user-controlled IPC name component for endpoints such as Windows
/// named pipes. Already-safe ASCII components are returned unchanged so
/// historical endpoint names remain stable. If any character must be replaced,
/// append a short hash of the original value so distinct unsafe names do not
/// collapse to the same pipe name.
#[must_use]
pub fn sanitize_ipc_component(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let sanitized: String = trimmed
        .chars()
        .map(|c| {
            if is_safe_ipc_component_char(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized == trimmed {
        return Some(sanitized);
    }
    let prefix: String = sanitized.chars().take(32).collect();
    Some(format!("{prefix}-{}", fnv_short_hash(trimmed.as_bytes())))
}

pub(super) fn is_safe_ipc_component_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'
}

/// Stable 8-hex-char identifier derived from the home dir's canonical
/// path. FNV-1a (64-bit) — small, deterministic, no extra dep.
pub(super) fn home_dir_short_hash(home: &Path) -> String {
    let canon = home.to_string_lossy();
    let canon = if cfg!(windows) {
        canon.to_ascii_lowercase()
    } else {
        canon.into_owned()
    };
    fnv_short_hash(canon.as_bytes())
}

pub(super) fn fnv_short_hash(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    // Take 32 bits → 8 hex chars. Plenty for collision avoidance at
    // per-machine scale.
    format!("{:08x}", (h ^ (h >> 32)) as u32)
}

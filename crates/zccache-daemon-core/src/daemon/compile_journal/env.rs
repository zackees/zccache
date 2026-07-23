//! Secret-safe environment capture for the durable compile journal.
//!
//! The compiler receives the request environment unchanged. This module owns
//! the narrower persistence contract: retain only build-diagnostic variables,
//! then reject secret-looking names and values even when they otherwise match
//! the allowlist.

use serde::Serialize;

const ALLOWED_EXACT: &[&str] = &[
    "AR",
    "CC",
    "CFLAGS",
    "CPPFLAGS",
    "CPATH",
    "CPLUS_INCLUDE_PATH",
    "CXX",
    "CXXFLAGS",
    "C_INCLUDE_PATH",
    "DEBUG",
    "HOST",
    "INCLUDE",
    "LD",
    "LDFLAGS",
    "LIB",
    "LIBRARY_PATH",
    "MACOSX_DEPLOYMENT_TARGET",
    "NUM_JOBS",
    "OPT_LEVEL",
    "OUT_DIR",
    "PROFILE",
    "RANLIB",
    "RUSTC",
    "RUSTC_WRAPPER",
    "RUSTDOC",
    "RUSTDOCFLAGS",
    "RUSTFLAGS",
    "RUSTUP_TOOLCHAIN",
    "SDKROOT",
    "STRIP",
    "TARGET",
    "CARGO_CRATE_NAME",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_MANIFEST_DIR",
    "CARGO_MANIFEST_PATH",
    "CARGO_PRIMARY_PACKAGE",
    "CARGO_TARGET_DIR",
    "ZCCACHE_PATH_REMAP",
    "ANDROID_HOME",
    "ANDROID_NDK_HOME",
    "IPHONEOS_DEPLOYMENT_TARGET",
    "VCINSTALLDIR",
    "WINDOWSSDKDIR",
];

const ALLOWED_PREFIXES: &[&str] = &["CARGO_CFG_", "CARGO_FEATURE_", "CARGO_PKG_", "DEP_"];

const SECRET_NAME_FRAGMENTS: &[&str] = &[
    "TOKEN",
    "PASSWORD",
    "PASSWD",
    "PASSPHRASE",
    "SECRET",
    "AUTH",
    "CREDENTIAL",
    "COOKIE",
    "PRIVATEKEY",
    "ACCESSKEY",
    "APIKEY",
    "SIGNINGKEY",
];

/// Return the durable, diagnostic-only subset of an environment.
///
/// Omission is intentional: no redaction marker is written because even a
/// variable name can disclose which credentials or services a build uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct SanitizedJournalEnv(Vec<(String, String)>);

impl SanitizedJournalEnv {
    #[must_use]
    pub fn as_slice(&self) -> &[(String, String)] {
        &self.0
    }
}

#[must_use]
pub fn sanitize_journal_env(env: Option<Vec<(String, String)>>) -> Option<SanitizedJournalEnv> {
    let sanitized: Vec<_> = env?
        .into_iter()
        .filter(|(name, value)| journal_env_pair_is_safe(name, value))
        .collect();
    (!sanitized.is_empty()).then_some(SanitizedJournalEnv(sanitized))
}

fn journal_env_pair_is_safe(name: &str, value: &str) -> bool {
    allowlisted_name(name) && !secret_name(name) && !secret_value(value)
}

fn allowlisted_name(name: &str) -> bool {
    if ALLOWED_EXACT.contains(&name)
        || ALLOWED_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
    {
        return true;
    }
    ALLOWED_EXACT
        .iter()
        .any(|allowed| name.eq_ignore_ascii_case(allowed))
        || ALLOWED_PREFIXES
            .iter()
            .any(|prefix| starts_with_ascii_case_insensitive(name, prefix))
}

fn secret_name(name: &str) -> bool {
    if SECRET_NAME_FRAGMENTS
        .iter()
        .any(|fragment| name.contains(fragment))
    {
        return true;
    }
    if SECRET_NAME_FRAGMENTS
        .iter()
        .any(|fragment| contains_ascii_case_insensitive(name, fragment))
    {
        return true;
    }

    // `KEY` is too short for a raw substring match (`MONKEY` is harmless),
    // but a component named KEY is a credential signal.
    ends_with_ascii_case_insensitive(name, "KEY")
        || name
            .split(|ch: char| !ch.is_ascii_alphanumeric())
            .any(|component| component.eq_ignore_ascii_case("KEY"))
}

fn secret_value(value: &str) -> bool {
    let trimmed = value.trim();

    if [
        "bearer ",
        "basic ",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "glpat-",
        "pypi-",
        "npm_",
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "xoxr-",
        "sk_live_",
        "rk_live_",
        "sk-",
        "aiza",
    ]
    .iter()
    .any(|marker| contains_ascii_case_insensitive(trimmed, marker))
    {
        return true;
    }

    if (contains_ascii_case_insensitive(trimmed, "-----begin ")
        && contains_ascii_case_insensitive(trimmed, "private key-----"))
        || [
            "token=",
            "password=",
            "secret=",
            "api_key=",
            "apikey=",
            "auth=",
        ]
        .iter()
        .any(|marker| contains_ascii_case_insensitive(trimmed, marker))
        || url_contains_credentials(trimmed)
        || secret_token_candidate(trimmed)
    {
        return true;
    }

    looks_like_opaque_token(trimmed)
}

fn url_contains_credentials(value: &str) -> bool {
    value
        .split_once("://")
        .and_then(|(_, authority_and_path)| authority_and_path.split('/').next())
        .is_some_and(|authority| authority.contains('@'))
}

fn secret_token_candidate(value: &str) -> bool {
    value
        .split(|ch: char| ch.is_ascii_whitespace() || matches!(ch, '\'' | '"' | ',' | ';'))
        .flat_map(|candidate| {
            let assigned = candidate.split_once('=').map(|(_, value)| value);
            std::iter::once(candidate).chain(assigned)
        })
        .any(|candidate| {
            looks_like_jwt(candidate)
                || looks_like_aws_access_key(candidate)
                || looks_like_opaque_token(candidate)
        })
}

fn looks_like_jwt(value: &str) -> bool {
    let mut segments = value.split('.');
    let Some(header) = segments.next() else {
        return false;
    };
    let Some(payload) = segments.next() else {
        return false;
    };
    let Some(signature) = segments.next() else {
        return false;
    };
    segments.next().is_none()
        && header.starts_with("eyJ")
        && payload.len() >= 8
        && signature.len() >= 8
        && [header, payload, signature]
            .iter()
            .all(|part| part.bytes().all(is_base64url_byte))
}

fn looks_like_aws_access_key(value: &str) -> bool {
    value.len() == 20
        && (value.starts_with("AKIA") || value.starts_with("ASIA"))
        && value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

fn looks_like_opaque_token(value: &str) -> bool {
    if !(32..=4096).contains(&value.len())
        || value.bytes().any(|byte| {
            byte.is_ascii_whitespace()
                || matches!(byte, b'/' | b'\\' | b':' | b';' | b',' | b'\'' | b'"')
        })
    {
        return false;
    }

    let mut has_alpha = false;
    let mut has_digit = false;
    for byte in value.bytes() {
        if byte.is_ascii_alphabetic() {
            has_alpha = true;
        } else if byte.is_ascii_digit() {
            has_digit = true;
        } else if !matches!(byte, b'_' | b'-' | b'.' | b'+' | b'=') {
            return false;
        }
    }
    has_alpha && has_digit
}

fn is_base64url_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
}

fn contains_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn starts_with_ascii_case_insensitive(value: &str, prefix: &str) -> bool {
    value
        .as_bytes()
        .get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix.as_bytes()))
}

fn ends_with_ascii_case_insensitive(value: &str, suffix: &str) -> bool {
    value
        .as_bytes()
        .get(value.len().saturating_sub(suffix.len())..)
        .is_some_and(|end| end.len() == suffix.len() && end.eq_ignore_ascii_case(suffix.as_bytes()))
}

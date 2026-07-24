//! Dylint-specific cache input validation.
//!
//! Dylint loads lint libraries dynamically, outside rustc's ordinary
//! dependency graph. A nested driver request is cacheable only when every
//! library named by `DYLINT_LIBS` can be content-hashed.

use std::path::Path;

use zccache_core::NormalizedPath;
use zccache_hash::ContentHash;

use crate::dylint_inner_rustc_args;

pub const DYLINT_LIBS_ENV: &str = "DYLINT_LIBS";
pub const DYLINT_CACHE_INPUT_HASH_ENV: &str = "ZCCACHE_DYLINT_CACHE_INPUT_HASH";

/// Whether an environment variable can affect Dylint diagnostics or driver
/// behavior and therefore belongs in both request and artifact identities.
#[must_use]
pub fn dylint_env_affects_output(name: &str) -> bool {
    (name.starts_with("DYLINT_") && name != DYLINT_LIBS_ENV)
        || matches!(name, "RUSTUP_HOME" | "RUSTUP_TOOLCHAIN")
        || name == "CLIPPY_DISABLE_DOCS_LINKS"
        || name == DYLINT_CACHE_INPUT_HASH_ENV
}

/// Validate and hash the non-rustc inputs to a nested Dylint request.
///
/// Returns `Ok(false)` for ordinary compilers. For Dylint, the function
/// parses `DYLINT_LIBS` as JSON, hashes every named library, and installs one
/// synthetic, non-replayed environment value that downstream request/context
/// key builders can consume. Any ambiguity is an error so callers can execute
/// the driver directly without caching.
pub fn prepare_dylint_cache_env(
    driver: &NormalizedPath,
    args: &[String],
    cwd: &Path,
    env: &mut Vec<(String, String)>,
) -> Result<bool, String> {
    let inner_rustc = match dylint_inner_rustc_args(driver.to_str().unwrap_or(""), args) {
        Ok(None) => return Ok(false),
        Ok(Some((inner_rustc, _))) => inner_rustc,
        Err(reason) => return Err(format!("{reason}; running uncached")),
    };
    let inner_rustc = resolve_input_path(inner_rustc, cwd);
    let driver_identity = hash_input(driver.as_path(), "Dylint driver")?;
    let inner_identity = hash_input(&inner_rustc, "Dylint inner rustc")?;
    prepare_dylint_cache_env_with_identities(
        driver,
        args,
        cwd,
        env,
        driver_identity,
        inner_identity,
        |path| hash_input(path, "Dylint library"),
    )
}

/// Cached-identity variant used by the daemon hot path.
///
/// The caller supplies the already memoized outer-driver and inner-rustc
/// identities, avoiding a full executable rehash for every compilation unit.
pub fn prepare_dylint_cache_env_with_identities<F>(
    driver: &NormalizedPath,
    args: &[String],
    cwd: &Path,
    env: &mut Vec<(String, String)>,
    driver_identity: ContentHash,
    inner_rustc_identity: ContentHash,
    mut hash_library: F,
) -> Result<bool, String>
where
    F: FnMut(&Path) -> Result<ContentHash, String>,
{
    match dylint_inner_rustc_args(driver.to_str().unwrap_or(""), args) {
        Ok(None) => return Ok(false),
        Ok(Some(_)) => {}
        Err(reason) => return Err(format!("{reason}; running uncached")),
    }

    env.retain(|(name, _)| name != DYLINT_CACHE_INPUT_HASH_ENV);
    let encoded = env
        .iter()
        .rev()
        .find_map(|(name, value)| (name == DYLINT_LIBS_ENV).then_some(value))
        .ok_or_else(|| format!("{DYLINT_LIBS_ENV} is missing; running uncached"))?;
    let libraries: Vec<NormalizedPath> = serde_json::from_str(encoded)
        .map_err(|error| format!("{DYLINT_LIBS_ENV} is invalid JSON: {error}; running uncached"))?;
    if libraries.is_empty() {
        return Err(format!(
            "{DYLINT_LIBS_ENV} names no lint libraries; running uncached"
        ));
    }

    let mut hasher = zccache_hash::StreamHasher::new();
    hasher.update(b"zccache-dylint-cache-input-v1\0");
    hasher.update(driver_identity.as_bytes());
    hasher.update(&[0]);
    hasher.update(inner_rustc_identity.as_bytes());
    hasher.update(&[0]);
    for library in libraries {
        let path = if library.as_path().is_absolute() {
            library
        } else {
            NormalizedPath::from(cwd.join(library.as_path()))
        };
        let library_name = path.as_path().file_name().ok_or_else(|| {
            format!(
                "Dylint library {} has no file name; running uncached",
                path.display()
            )
        })?;
        let content = hash_library(path.as_path())?;
        hasher.update(library_name.to_string_lossy().as_bytes());
        hasher.update(b"=");
        hasher.update(content.as_bytes());
        hasher.update(&[0]);
    }

    let mut output_env: Vec<(&str, &str)> = env
        .iter()
        .filter(|(name, _)| dylint_env_affects_output(name))
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    output_env.sort_unstable();
    for (name, value) in output_env {
        hasher.update(name.as_bytes());
        hasher.update(b"=");
        hasher.update(value.as_bytes());
        hasher.update(&[0]);
    }

    env.push((
        DYLINT_CACHE_INPUT_HASH_ENV.to_string(),
        hasher.finalize().to_hex(),
    ));
    Ok(true)
}

fn resolve_input_path(input: &str, cwd: &Path) -> NormalizedPath {
    let path = Path::new(input);
    if path.is_absolute() {
        NormalizedPath::from(path)
    } else {
        NormalizedPath::from(cwd.join(path))
    }
}

fn hash_input(path: &Path, kind: &str) -> Result<ContentHash, String> {
    zccache_hash::hash_file(path).map_err(|error| {
        format!(
            "cannot hash {kind} {}: {error}; running uncached",
            path.display()
        )
    })
}

//! `zccache cargo-registry` subcommands: save / restore / hash / clean.

use crate::core::NormalizedPath;
use std::process::ExitCode;

use super::util::{env_flag_truthy, format_bytes};

/// Resolve the cargo home directory from an explicit argument, the `CARGO_HOME`
/// env var, or the default `~/.cargo`.
pub(crate) fn resolve_cargo_home(explicit: Option<&str>) -> Result<NormalizedPath, String> {
    if let Some(p) = explicit {
        return Ok(NormalizedPath::from(p));
    }
    if let Ok(ch) = std::env::var("CARGO_HOME") {
        if !ch.is_empty() {
            return Ok(NormalizedPath::from(ch));
        }
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| "cannot determine home directory (set HOME or CARGO_HOME)".to_string())?;
    Ok(NormalizedPath::from(home).join(".cargo"))
}

/// Directory where cargo-registry archives are stored.
///
/// This is rooted at the same cache root reported by `zccache cache-root` so
/// soldr/setup-soldr can redirect the full zccache-owned cache surface with
/// `ZCCACHE_CACHE_DIR`.
pub(crate) fn cargo_registry_cache_dir() -> NormalizedPath {
    crate::core::config::cargo_registry_cache_dir()
}

pub(crate) fn cmd_cargo_registry_save(key: &str, cargo_home: Option<&str>) -> ExitCode {
    // setup-soldr#70's payload C migration: when setup-soldr owns
    // `~/.cargo/registry` caching with fast-zstd, double-saving here just
    // burns CPU on the same bytes. Caller signals takeover via env var.
    if env_flag_truthy("SOLDR_SKIP_CARGO_REGISTRY_SAVE") {
        println!(
            "cargo-registry save: skipping (SOLDR_SKIP_CARGO_REGISTRY_SAVE=1) \
             — caller owns the cache layer"
        );
        return ExitCode::SUCCESS;
    }
    let cargo_home = match resolve_cargo_home(cargo_home) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("zccache cargo-registry save: {e}");
            return ExitCode::FAILURE;
        }
    };
    let cache_dir = cargo_registry_cache_dir();
    if let Err(e) = std::fs::create_dir_all(&cache_dir) {
        eprintln!(
            "zccache cargo-registry save: failed to create {}: {e}",
            cache_dir.display()
        );
        return ExitCode::FAILURE;
    }
    let archive_path = cache_dir.join(format!("{key}.tar.gz"));

    // Collect paths to archive.
    let subdirs: &[&str] = &["registry/index", "registry/cache", "git/db"];
    let mut paths: Vec<(NormalizedPath, String)> = Vec::new();
    for subdir in subdirs {
        let p = cargo_home.join(subdir);
        if p.exists() {
            paths.push((p, subdir.to_string()));
        }
    }

    if paths.is_empty() {
        eprintln!(
            "no cargo registry directories found in {}",
            cargo_home.display()
        );
        return ExitCode::SUCCESS;
    }

    // Create tar.gz archive.
    let file = match std::fs::File::create(&archive_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "zccache cargo-registry save: failed to create {}: {e}",
                archive_path.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let gz = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    let mut tar = tar::Builder::new(gz);

    for (path, name) in &paths {
        if let Err(e) = tar.append_dir_all(name, path) {
            eprintln!("zccache cargo-registry save: failed to add {name}: {e}");
            return ExitCode::FAILURE;
        }
    }
    if let Err(e) = tar.finish() {
        eprintln!("zccache cargo-registry save: failed to finalize archive: {e}");
        return ExitCode::FAILURE;
    }

    let size = std::fs::metadata(&archive_path)
        .map(|m| m.len())
        .unwrap_or(0);
    println!(
        "saved cargo registry to {} ({})",
        archive_path.display(),
        format_bytes(size)
    );
    ExitCode::SUCCESS
}

pub(crate) fn cmd_cargo_registry_restore(key: &str, cargo_home: Option<&str>) -> ExitCode {
    let cargo_home = match resolve_cargo_home(cargo_home) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("zccache cargo-registry restore: {e}");
            return ExitCode::FAILURE;
        }
    };
    let cache_dir = cargo_registry_cache_dir();
    let archive_path = cache_dir.join(format!("{key}.tar.gz"));

    if !archive_path.exists() {
        eprintln!("no cached registry found for key: {key}");
        return ExitCode::FAILURE;
    }

    let file = match std::fs::File::open(&archive_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "zccache cargo-registry restore: failed to open {}: {e}",
                archive_path.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let gz = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(gz);
    if let Err(e) = tar.unpack(&cargo_home) {
        eprintln!("zccache cargo-registry restore: failed to unpack archive: {e}");
        return ExitCode::FAILURE;
    }

    println!("restored cargo registry from {}", archive_path.display());
    ExitCode::SUCCESS
}

pub(crate) fn cmd_cargo_registry_hash(lockfile: &str) -> ExitCode {
    let path = std::path::Path::new(lockfile);
    if !path.exists() {
        eprintln!("lockfile not found: {lockfile}");
        return ExitCode::FAILURE;
    }
    let hash = match crate::hash::hash_file(path) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("zccache cargo-registry hash: failed to hash {lockfile}: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Print first 16 hex chars (matches action's cache key format).
    let hex = hash.to_hex();
    println!("{}", &hex[..16]);
    ExitCode::SUCCESS
}

pub(crate) fn cmd_cargo_registry_clean() -> ExitCode {
    let cache_dir = cargo_registry_cache_dir();
    if cache_dir.exists() {
        let count = match std::fs::read_dir(&cache_dir) {
            Ok(entries) => entries.count(),
            Err(e) => {
                eprintln!(
                    "zccache cargo-registry clean: failed to read {}: {e}",
                    cache_dir.display()
                );
                return ExitCode::FAILURE;
            }
        };
        if let Err(e) = std::fs::remove_dir_all(&cache_dir) {
            eprintln!(
                "zccache cargo-registry clean: failed to remove {}: {e}",
                cache_dir.display()
            );
            return ExitCode::FAILURE;
        }
        println!("removed {count} cached registry archive(s)");
    } else {
        println!("no cached archives to clean");
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_registry_command_uses_core_cache_layout() {
        assert_eq!(
            cargo_registry_cache_dir(),
            crate::core::config::cargo_registry_cache_dir()
        );
    }
}

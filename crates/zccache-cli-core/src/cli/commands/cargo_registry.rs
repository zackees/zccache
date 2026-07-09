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

pub(crate) fn cmd_cargo_registry_save(
    key: &str,
    cargo_home: Option<&str>,
    output: Option<&str>,
) -> ExitCode {
    // setup-soldr#70's payload C migration: when setup-soldr owns
    // `~/.cargo/registry` caching with fast-zstd, double-saving to the
    // default location burns CPU on bytes setup-soldr already owns.
    // Caller signals takeover via `SOLDR_SKIP_CARGO_REGISTRY_SAVE=1`.
    //
    // BUT: when the caller passes `--output PATH`, they are explicitly
    // directing the archive to a non-standard location — the
    // setup-soldr coordination flag does not apply, so we bypass the
    // skip and run the save. (Without this carve-out, a CI smoke test
    // or any other explicit consumer that runs inside a setup-soldr
    // context would have to `unset` the env var to make `save` do
    // anything, which is brittle and easy to miss.)
    if output.is_none() && env_flag_truthy("SOLDR_SKIP_CARGO_REGISTRY_SAVE") {
        println!(
            "cargo-registry save: skipping (SOLDR_SKIP_CARGO_REGISTRY_SAVE=1) \
             — caller owns the cache layer (pass --output to override)"
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
    // Resolve the archive destination. `--output` takes the full path
    // (including filename); without it we fall back to the derived
    // `<cache-root>/cargo-registry/<key>.tar.gz`.
    let archive_path: std::path::PathBuf = match output {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let cache_dir = cargo_registry_cache_dir();
            if let Err(e) = std::fs::create_dir_all(&cache_dir) {
                eprintln!(
                    "zccache cargo-registry save: failed to create {}: {e}",
                    cache_dir.display()
                );
                return ExitCode::FAILURE;
            }
            cache_dir.join(format!("{key}.tar.gz")).into_path_buf()
        }
    };
    // For `--output`, the caller may have given us a path whose parent
    // directory does not exist yet. Create it on their behalf so we
    // match the default-path branch's create-on-demand behavior.
    if let Some(parent) = archive_path.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!(
                    "zccache cargo-registry save: failed to create {}: {e}",
                    parent.display()
                );
                return ExitCode::FAILURE;
            }
        }
    }

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

    /// Process-shared env-var helper. The save fn reads
    /// `SOLDR_SKIP_CARGO_REGISTRY_SAVE` from process env; the tests
    /// below toggle it and must not race each other.
    fn with_skip_env<F: FnOnce()>(set: bool, f: F) {
        let var = "SOLDR_SKIP_CARGO_REGISTRY_SAVE";
        let prev = std::env::var_os(var);
        // SAFETY: documented in test-local guard; restored below.
        unsafe {
            if set {
                std::env::set_var(var, "1");
            } else {
                std::env::remove_var(var);
            }
        }
        f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var(var, v),
                None => std::env::remove_var(var),
            }
        }
    }

    /// `--output` suppresses the `SOLDR_SKIP_CARGO_REGISTRY_SAVE` skip:
    /// the caller has explicitly chosen a non-standard destination, so
    /// the env var's coordination intent does not apply. The
    /// observable contract is the archive file appearing at the
    /// explicit path; without the bypass, the save would short-circuit
    /// and the file would not exist.
    #[test]
    fn output_path_bypasses_soldr_skip_env() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let archive = tmp.path().join("explicit.tar.gz");
        let archive_str = archive.to_string_lossy().into_owned();

        with_skip_env(true, || {
            // Use a small fake cargo home so the save has *something*
            // to archive instead of erroring out on an empty registry.
            let fake_cargo = tmp.path().join("cargo");
            let reg = fake_cargo.join("registry").join("index");
            std::fs::create_dir_all(&reg).unwrap();
            std::fs::write(reg.join("marker"), b"x").unwrap();
            let _ = cmd_cargo_registry_save(
                "ignored-when-output-set",
                Some(&fake_cargo.to_string_lossy()),
                Some(&archive_str),
            );
            assert!(
                archive.is_file(),
                "archive must be written at the explicit --output path \
                 (env var should not have skipped this): {}",
                archive.display()
            );
        });
    }

    /// Without `--output`, `SOLDR_SKIP_CARGO_REGISTRY_SAVE=1` keeps the
    /// pre-existing short-circuit behavior — the setup-soldr
    /// coordination flag continues to apply when the caller is using
    /// the default derived path.
    #[test]
    fn default_path_still_respects_soldr_skip_env() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Redirect zccache's cache root into the tempdir so the test
        // does not touch the real home directory if the save WERE to
        // run (it should not, per the env var).
        let prev_cache_dir = std::env::var_os("ZCCACHE_CACHE_DIR");
        unsafe { std::env::set_var("ZCCACHE_CACHE_DIR", tmp.path()) };

        with_skip_env(true, || {
            let _ = cmd_cargo_registry_save("noop-key", None, None);
            // The skip path returns before touching the disk, so no
            // archive should appear anywhere under the redirected root.
            let mut stray = Vec::new();
            walk_collect_gz(tmp.path(), &mut stray);
            assert!(
                stray.is_empty(),
                "skip path must not write any .gz archive; found: {stray:?}"
            );
        });

        unsafe {
            match prev_cache_dir {
                Some(v) => std::env::set_var("ZCCACHE_CACHE_DIR", v),
                None => std::env::remove_var("ZCCACHE_CACHE_DIR"),
            }
        }
    }

    /// Shallow recursive walk for tests — avoid adding a `walkdir` dep
    /// just for this one assertion.
    fn walk_collect_gz(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk_collect_gz(&p, out);
            } else if p.extension().is_some_and(|e| e == "gz") {
                out.push(p);
            }
        }
    }
}

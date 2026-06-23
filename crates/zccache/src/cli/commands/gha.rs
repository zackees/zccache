//! `zccache gha-cache` subcommands: status / save / restore.

use crate::core::NormalizedPath;
use crate::gha::{GhaCache, GhaError};
use std::path::Path;
use std::process::ExitCode;

use super::targz::{tar_gz_decode_async, tar_gz_encode_async};

pub(crate) fn cmd_gha_status() -> ExitCode {
    if GhaCache::is_available() {
        let url = std::env::var("ACTIONS_CACHE_URL").unwrap_or_default();
        println!("GHA cache: available");
        println!("  ACTIONS_CACHE_URL = {url}");
        ExitCode::SUCCESS
    } else {
        println!("GHA cache: not available (ACTIONS_CACHE_URL or ACTIONS_RUNTIME_TOKEN not set)");
        ExitCode::SUCCESS
    }
}

pub(crate) async fn cmd_gha_save(key: &str, path: &str) -> ExitCode {
    let cache = match GhaCache::from_env() {
        Ok(c) => c,
        Err(GhaError::NotAvailable) => {
            eprintln!("zccache gha-cache: not running in GitHub Actions");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("zccache gha-cache: {e}");
            return ExitCode::FAILURE;
        }
    };

    let src = Path::new(path);
    if !src.exists() {
        eprintln!("zccache gha-cache save: path does not exist: {path}");
        return ExitCode::FAILURE;
    }

    let data = match tar_gz_encode_async(NormalizedPath::new(src)).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("zccache gha-cache save: failed to create archive: {e}");
            return ExitCode::FAILURE;
        }
    };

    let version = GhaCache::version_hash(&[path]);
    match cache.save(key, &version, &data).await {
        Ok(()) => {
            eprintln!(
                "zccache gha-cache save: uploaded {} bytes for key '{key}'",
                data.len()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("zccache gha-cache save: {e}");
            ExitCode::FAILURE
        }
    }
}

pub(crate) async fn cmd_gha_restore(key: &str, path: &str) -> ExitCode {
    let cache = match GhaCache::from_env() {
        Ok(c) => c,
        Err(GhaError::NotAvailable) => {
            eprintln!("zccache gha-cache: not running in GitHub Actions");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("zccache gha-cache: {e}");
            return ExitCode::FAILURE;
        }
    };

    let version = GhaCache::version_hash(&[path]);
    let data = match cache.restore(key, &version).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            eprintln!("zccache gha-cache restore: cache miss for key '{key}'");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("zccache gha-cache restore: {e}");
            return ExitCode::FAILURE;
        }
    };

    let dest = Path::new(path);
    if let Err(e) = tokio::fs::create_dir_all(dest).await {
        eprintln!("zccache gha-cache restore: failed to create directory: {e}");
        return ExitCode::FAILURE;
    }

    let restored_bytes = data.len();
    match tar_gz_decode_async(data, NormalizedPath::new(dest)).await {
        Ok(()) => {
            eprintln!(
                "zccache gha-cache restore: restored {} bytes for key '{key}' to {path}",
                restored_bytes
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("zccache gha-cache restore: failed to extract archive: {e}");
            ExitCode::FAILURE
        }
    }
}

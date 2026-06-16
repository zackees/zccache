use std::path::{Path, PathBuf};

use crate::download::{canonical_destination, DownloadOptions};

use super::{ArchiveFormat, DownloadSource, FetchRequest, WaitMode};

#[derive(Debug, Clone)]
pub(super) struct ResolvedFetchRequest {
    pub(super) source: DownloadSource,
    pub(super) cache_path: PathBuf,
    pub(super) expanded_path: Option<PathBuf>,
    pub(super) expected_sha256: Option<String>,
    pub(super) archive_format: ArchiveFormat,
    pub(super) wait_mode: WaitMode,
    pub(super) dry_run: bool,
    pub(super) force: bool,
    pub(super) download_options: DownloadOptions,
}

pub(super) fn resolve_request(request: &FetchRequest) -> Result<ResolvedFetchRequest, String> {
    Ok(ResolvedFetchRequest {
        source: normalize_source(request.source.clone())?,
        cache_path: canonical_destination(&request.destination_path)
            .map_err(|e| e.to_string())?
            .into_path_buf(),
        expanded_path: request
            .destination_path_expanded
            .as_ref()
            .map(|p| normalize_target(p, true))
            .transpose()?,
        expected_sha256: request.expected_sha256.clone().map(normalize_sha256),
        archive_format: request.archive_format,
        wait_mode: request.wait_mode,
        dry_run: request.dry_run,
        force: request.force,
        download_options: request.download_options.clone(),
    })
}

pub(super) fn resolve_request_no_create(
    request: &FetchRequest,
) -> Result<ResolvedFetchRequest, String> {
    Ok(ResolvedFetchRequest {
        source: normalize_source(request.source.clone())?,
        cache_path: normalize_target(&request.destination_path, false)?,
        expanded_path: request
            .destination_path_expanded
            .as_ref()
            .map(|p| normalize_target(p, false))
            .transpose()?,
        expected_sha256: request.expected_sha256.clone().map(normalize_sha256),
        archive_format: request.archive_format,
        wait_mode: request.wait_mode,
        dry_run: request.dry_run,
        force: request.force,
        download_options: request.download_options.clone(),
    })
}

fn normalize_target(path: &Path, create_parent: bool) -> Result<PathBuf, String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(path)
    };
    let file_name = absolute
        .file_name()
        .map(ToOwned::to_owned)
        .ok_or_else(|| "path must include a terminal file or directory name".to_string())?;
    let parent = absolute.parent().unwrap_or_else(|| Path::new("."));
    let canonical_parent = if parent.exists() {
        std::fs::canonicalize(parent).map_err(|e| e.to_string())?
    } else if create_parent {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        std::fs::canonicalize(parent).map_err(|e| e.to_string())?
    } else {
        crate::core::NormalizedPath::new(parent).into_path_buf()
    };
    Ok(canonical_parent.join(file_name))
}

fn normalize_sha256(value: String) -> String {
    value.trim().to_ascii_lowercase()
}

fn normalize_source(source: DownloadSource) -> Result<DownloadSource, String> {
    match source {
        DownloadSource::Url(url) => {
            if url.trim().is_empty() {
                Err("download source URL must not be empty".to_string())
            } else {
                Ok(DownloadSource::Url(url))
            }
        }
        DownloadSource::MultipartUrls(urls) => {
            if urls.is_empty() {
                return Err("multipart download source must include at least one URL".to_string());
            }
            if urls.iter().any(|url| url.trim().is_empty()) {
                return Err("multipart download source contains an empty URL".to_string());
            }
            Ok(DownloadSource::MultipartUrls(urls))
        }
    }
}

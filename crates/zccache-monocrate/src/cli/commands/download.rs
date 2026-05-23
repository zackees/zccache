//! `zccache download` — single-file or multipart download via the daemon.

use std::process::ExitCode;

use super::super::{client_download, DownloadParams, DownloadSource};

pub(crate) fn cmd_download(params: DownloadParams) -> ExitCode {
    match client_download(None, params) {
        Ok(result) => {
            println!("status={:?}", result.status);
            println!("archive_path={}", result.cache_path.display());
            println!("sha256={}", result.sha256);
            if let Some(unarchive_path) = &result.expanded_path {
                println!("unarchive_path={}", unarchive_path.display());
            }
            if let Some(bytes) = result.bytes {
                println!("bytes={bytes}");
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("zccache download: {err}");
            ExitCode::FAILURE
        }
    }
}

pub(crate) fn resolve_download_source(
    url: Option<String>,
    part_urls: Vec<String>,
) -> Result<DownloadSource, String> {
    match (url, part_urls.is_empty()) {
        (Some(url), true) => Ok(DownloadSource::Url(url)),
        (None, false) => Ok(DownloadSource::MultipartUrls(part_urls)),
        (Some(_), false) => Err("use either --url or --part-url, not both".to_string()),
        (None, true) => Err("provide either --url or at least one --part-url".to_string()),
    }
}

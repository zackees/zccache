use std::path::Path;

use reqwest::header::ACCEPT_ENCODING;
use tokio::io::AsyncWriteExt;

use super::hashing::temp_download_path;

pub(super) fn download_explicit_parts(
    part_urls: &[String],
    destination: &Path,
) -> Result<(), String> {
    let temp_path = temp_download_path(destination);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to create tokio runtime: {e}"))?;
    runtime.block_on(async move {
        let client = reqwest::Client::builder()
            .user_agent(format!("zccache-download/{}", crate::core::VERSION))
            .build()
            .map_err(|e| e.to_string())?;

        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| e.to_string())?;
        }

        let _ = tokio::fs::remove_file(&temp_path).await;

        let result = async {
            let mut output = tokio::fs::File::create(&temp_path)
                .await
                .map_err(|e| e.to_string())?;
            for url in part_urls {
                let mut response = client
                    .get(url)
                    .header(ACCEPT_ENCODING, "identity")
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                if !response.status().is_success() {
                    return Err(format!("unexpected status {} for {url}", response.status()));
                }
                while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
                    output.write_all(&chunk).await.map_err(|e| e.to_string())?;
                }
            }
            output.flush().await.map_err(|e| e.to_string())?;
            drop(output);
            if destination.exists() {
                let _ = tokio::fs::remove_file(destination).await;
            }
            tokio::fs::rename(&temp_path, destination)
                .await
                .map_err(|e| e.to_string())
        }
        .await;

        if result.is_err() {
            let _ = tokio::fs::remove_file(&temp_path).await;
        }
        result
    })
}

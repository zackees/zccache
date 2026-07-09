use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Errors from the GitHub Actions cache client.
#[derive(Error, Debug)]
pub enum GhaError {
    #[error("GHA cache API not available (ACTIONS_CACHE_URL or ACTIONS_RUNTIME_TOKEN not set)")]
    NotAvailable,
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("API error: {status} {body}")]
    Api { status: u16, body: String },
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Cache not found for key: {0}")]
    NotFound(String),
}

/// Client for the GitHub Actions Cache API.
///
/// Reads `ACTIONS_CACHE_URL` and `ACTIONS_RUNTIME_TOKEN` from the environment
/// (both are set automatically by the GHA runner).
#[derive(Debug)]
pub struct GhaCache {
    client: Client,
    base_url: String,
    token: String,
}

#[derive(Serialize)]
struct ReserveCacheRequest {
    key: String,
    version: String,
}

#[derive(Deserialize)]
struct ReserveCacheResponse {
    #[serde(rename = "cacheId")]
    cache_id: i64,
}

#[derive(Serialize)]
struct CommitCacheRequest {
    size: u64,
}

#[derive(Deserialize)]
struct RestoreCacheResponse {
    #[serde(rename = "archiveLocation")]
    archive_location: Option<String>,
    #[serde(rename = "cacheKey")]
    #[allow(dead_code)]
    cache_key: Option<String>,
}

/// API version header value used by the GHA cache REST API.
const API_VERSION: &str = "application/json;api-version=6.0-preview.1";

impl GhaCache {
    /// Create a new GHA cache client from environment variables.
    ///
    /// Returns `Err(GhaError::NotAvailable)` when not running inside GitHub
    /// Actions (i.e., the required env vars are missing).
    pub fn from_env() -> Result<Self, GhaError> {
        let base_url = std::env::var("ACTIONS_CACHE_URL").map_err(|_| GhaError::NotAvailable)?;
        let token = std::env::var("ACTIONS_RUNTIME_TOKEN").map_err(|_| GhaError::NotAvailable)?;

        let client = Client::builder()
            .user_agent("zccache")
            .build()
            .map_err(GhaError::Http)?;

        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
        })
    }

    /// Check whether GHA cache env vars are present.
    pub fn is_available() -> bool {
        std::env::var("ACTIONS_CACHE_URL").is_ok() && std::env::var("ACTIONS_RUNTIME_TOKEN").is_ok()
    }

    fn api_url(&self, path: &str) -> String {
        format!("{}/_apis/artifactcache/{}", self.base_url, path)
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.token)
    }

    /// Compute a deterministic version hash from a set of path strings.
    ///
    /// The GHA cache API requires a `version` field that differentiates
    /// caches that share the same key but cover different paths.
    pub fn version_hash(paths: &[&str]) -> String {
        let mut hasher = Sha256::new();
        for p in paths {
            hasher.update(p.as_bytes());
            hasher.update(b"|");
        }
        format!("{:x}", hasher.finalize())
    }

    /// Save a blob to the GHA cache under the given key and version.
    ///
    /// The three-step protocol is: reserve -> upload -> commit.
    /// If the cache key already exists (HTTP 409) the call succeeds silently.
    pub async fn save(&self, key: &str, version: &str, data: &[u8]) -> Result<(), GhaError> {
        // Step 1: Reserve a cache entry.
        let reserve_url = self.api_url("caches");
        let resp = self
            .client
            .post(&reserve_url)
            .header("Authorization", self.auth_header())
            .header("Accept", API_VERSION)
            .json(&ReserveCacheRequest {
                key: key.to_string(),
                version: version.to_string(),
            })
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            // 409 Conflict = cache already exists, not an error.
            if status == 409 {
                tracing::info!("cache already exists for key: {key}");
                return Ok(());
            }
            return Err(GhaError::Api { status, body });
        }

        let reserve: ReserveCacheResponse = resp.json().await?;
        let cache_id = reserve.cache_id;

        // Step 2: Upload the data in a single chunk.
        let upload_url = self.api_url(&format!("caches/{cache_id}"));
        let len = data.len();
        let content_range = if len == 0 {
            "bytes */*".to_string()
        } else {
            format!("bytes 0-{}/{len}", len - 1)
        };
        let resp = self
            .client
            .patch(&upload_url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/octet-stream")
            .header("Content-Range", content_range)
            .body(data.to_vec())
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(GhaError::Api { status, body });
        }

        // Step 3: Commit (finalize) the cache entry.
        let commit_url = self.api_url(&format!("caches/{cache_id}"));
        let resp = self
            .client
            .post(&commit_url)
            .header("Authorization", self.auth_header())
            .header("Accept", API_VERSION)
            .json(&CommitCacheRequest { size: len as u64 })
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(GhaError::Api { status, body });
        }

        tracing::info!("saved {len} bytes to GHA cache key: {key}");
        Ok(())
    }

    /// Restore a blob from the GHA cache. Returns `None` if no entry was found.
    pub async fn restore(&self, key: &str, version: &str) -> Result<Option<Vec<u8>>, GhaError> {
        let url = self.api_url(&format!("cache?keys={key}&version={version}"));
        let resp = self
            .client
            .get(&url)
            .header("Authorization", self.auth_header())
            .header("Accept", API_VERSION)
            .send()
            .await?;

        // 204 No Content = cache miss.
        if resp.status().as_u16() == 204 {
            return Ok(None);
        }

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(GhaError::Api { status, body });
        }

        let result: RestoreCacheResponse = resp.json().await?;
        let location = match result.archive_location {
            Some(loc) => loc,
            None => return Ok(None),
        };

        // Download the blob from the archive location.
        let data = self.client.get(&location).send().await?.bytes().await?;

        tracing::info!("restored {} bytes from GHA cache key: {key}", data.len());
        Ok(Some(data.to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        old_cache_url: Option<String>,
        old_runtime_token: Option<String>,
    }

    impl EnvGuard {
        fn new() -> Self {
            Self {
                _lock: env_lock().lock().unwrap(),
                old_cache_url: std::env::var("ACTIONS_CACHE_URL").ok(),
                old_runtime_token: std::env::var("ACTIONS_RUNTIME_TOKEN").ok(),
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.old_cache_url {
                Some(value) => std::env::set_var("ACTIONS_CACHE_URL", value),
                None => std::env::remove_var("ACTIONS_CACHE_URL"),
            }
            match &self.old_runtime_token {
                Some(value) => std::env::set_var("ACTIONS_RUNTIME_TOKEN", value),
                None => std::env::remove_var("ACTIONS_RUNTIME_TOKEN"),
            }
        }
    }

    #[test]
    fn is_available_returns_false_without_env_vars() {
        let _guard = EnvGuard::new();
        // Clear env vars in case they happen to be set.
        std::env::remove_var("ACTIONS_CACHE_URL");
        std::env::remove_var("ACTIONS_RUNTIME_TOKEN");
        assert!(!GhaCache::is_available());
    }

    #[test]
    fn version_hash_is_deterministic() {
        let h1 = GhaCache::version_hash(&["a", "b", "c"]);
        let h2 = GhaCache::version_hash(&["a", "b", "c"]);
        assert_eq!(h1, h2);
        assert!(!h1.is_empty());
    }

    #[test]
    fn version_hash_differs_for_different_inputs() {
        let h1 = GhaCache::version_hash(&["a", "b"]);
        let h2 = GhaCache::version_hash(&["a", "c"]);
        assert_ne!(h1, h2);
    }

    #[test]
    fn from_env_returns_not_available_without_env_vars() {
        let _guard = EnvGuard::new();
        std::env::remove_var("ACTIONS_CACHE_URL");
        std::env::remove_var("ACTIONS_RUNTIME_TOKEN");
        let err = GhaCache::from_env().unwrap_err();
        assert!(
            matches!(err, GhaError::NotAvailable),
            "expected NotAvailable, got: {err}"
        );
    }

    #[test]
    fn from_env_returns_not_available_with_partial_env() {
        let _guard = EnvGuard::new();
        // Only one of the two vars set.
        std::env::set_var("ACTIONS_CACHE_URL", "https://example.com");
        std::env::remove_var("ACTIONS_RUNTIME_TOKEN");
        let err = GhaCache::from_env().unwrap_err();
        assert!(matches!(err, GhaError::NotAvailable));
    }

    #[test]
    fn from_env_succeeds_with_both_env_vars() {
        let _guard = EnvGuard::new();
        std::env::set_var("ACTIONS_CACHE_URL", "https://example.com/cache/");
        std::env::set_var("ACTIONS_RUNTIME_TOKEN", "test-token");
        let cache = GhaCache::from_env().expect("should succeed with both vars set");
        // Verify trailing slash is stripped.
        assert_eq!(cache.base_url, "https://example.com/cache");
        assert_eq!(cache.token, "test-token");
    }
}

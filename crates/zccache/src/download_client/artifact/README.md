# `download_client::artifact`

Artifact fetch pipeline for the download client. Resolves a `FetchRequest`,
acquires a cross-process lock, downloads (single URL or explicit multipart),
validates fingerprints, writes state markers, and optionally extracts an
archive into an expanded directory.

## Submodules

| File | Responsibility |
|---|---|
| `mod.rs` | Public types (`FetchRequest`, `FetchResult`, `FetchState`, `WaitMode`, `ArchiveFormat`, `FetchStatus`, `FetchStateKind`, `DownloadSource`) and `impl DownloadClient::{fetch, exists}` orchestration |
| `resolve.rs` | `ResolvedFetchRequest` plus `resolve_request*` / `normalize_*` helpers |
| `state.rs` | `exists_resolved`, `validate_artifact`, `cleanup_invalid_fetch_state`, `artifact_matches_request`, `read_or_compute_artifact_fingerprint` |
| `parts.rs` | `download_explicit_parts` — sequential multipart download |
| `hashing.rs` | `sha256_file`, `compute_artifact_fingerprint`, `temp_download_path`, `ArtifactFingerprint` |
| `lock.rs` | `FetchLock`, `acquire_fetch_lock`, `fetch_lock_path` (file locks under the cache dir) |
| `marker.rs` | `ArtifactMarker` / `ExpandedMarker` JSON sidecars plus their path/read/write helpers |
| `archive.rs` | Format detection (`detect_archive_format`, `auto_archive_format`), extractors for zst/xz/zip/tar.gz/tar.xz/tar.zst/7z, plus `safe_join`, `copy_file`, `write_decoded_to_file`, `remove_path_if_exists` |
| `tests/` | Unit tests for archive helpers plus end-to-end integration tests against a local HTTP server + `TestDaemon` |

All `pub` re-exports remain at `download_client::artifact::<Name>` so the
parent `download_client/mod.rs` re-export surface is unchanged.

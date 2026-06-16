# `download_client::artifact::tests`

Unit and integration tests for the artifact fetch pipeline.

- `mod.rs` — shared test helpers (`TestHttpServer`, `TestDaemon`,
  `fetch_with_retry`, etc.) plus small unit tests for archive helpers
  (`auto_detect_archive_formats`, `safe_join_rejects_parent_traversal`,
  `zip_extraction_rejects_path_traversal`, `tar_gz_extracts_regular_files`).
- `fetch.rs` — end-to-end `DownloadClient::fetch` / `exists` tests against a
  local HTTP server and a per-test `DownloadDaemon`.

# core::config

Configuration types and cache-root resolution for zccache.

Split into focused submodules (file-size discipline: every source file < 1,000 LOC):

- **`mod.rs`** — Public `Config` struct + `Default` impl, top-level constants
  (`CACHE_DIR_ENV`, `DAEMON_NAMESPACE_ENV`, `DEFAULT_IDLE_TIMEOUT_SECS`,
  `COLOCATE_ENV`, etc.), and `pub use` re-exports of every public symbol so the
  external path `crate::core::config::<Name>` is preserved.
- **`resolve.rs`** — Cache-root resolution: `default_cache_dir`,
  `resolve_cache_root`, `resolve_cache_root_top_level`, `versioned_subdir`,
  `CacheRootSource`, `cache_dir_override`, and the cross-volume colocation
  logic (`ZCCACHE_COLOCATE`, `volume_root`, `same_volume_root`).
- **`paths.rs`** — Well-known subpath helpers under the cache root
  (`artifacts_dir`, `tmp_dir`, `depfile_dir`, `depgraph_dir`, `log_dir`,
  `crash_dump_dir`, `index_path`, `metadata_path_from_cache_dir`,
  `symbols_cache_dir`, `cargo_registry_cache_dir`, etc.).
- **`namespace.rs`** — `ZCCACHE_DAEMON_NAMESPACE` parsing
  (`daemon_namespace`, `daemon_namespace_label`), IPC sanitization
  (`sanitize_ipc_component`, `sanitize_daemon_namespace`), and the FNV-1a
  short-hash helper reused by colocation.
- **`cleanup.rs`** — Legacy / stale temp-state cleanup
  (`cleanup_legacy_temp_root_state`, `cleanup_stale_depfile_dirs`).
- **`tests.rs`** — Unit tests for all of the above (kept here because they
  exercise private helpers across modules).

See `docs/architecture/runtime.md § Cache root invariants` for the
`ZCCACHE_CACHE_DIR` contract and issue #761 / #762 (per-daemon-version
namespacing) for the rationale behind `versioned_subdir()`.

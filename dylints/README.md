# Dylint Libraries

Custom Rust lints used by this workspace.

- `ban_std_pathbuf`: bans new uses of `std::path::PathBuf` outside the explicit legacy allowlist.
- `ban_unrooted_tempdir`: bans tempdir/temp-file creation under `$TMPDIR` instead of `zccache_core::config::default_cache_dir()`.
- `ban_raw_subprocess_in_daemon`: bans raw subprocess spawns in daemon code paths.
- `ban_tmp_literal`: bans hardcoded `/tmp` path string literals — they only exist on POSIX (#828).
- `ban_legacy_artifact_path`: bans ad-hoc reconstruction of the flat-v1 `<key>_<index>` artifact filename convention outside the artifact-layout owner.
- `ban_normalized_path_deref_containment`: bans `std::path::Path` containment methods (`starts_with`, `strip_prefix`) resolved through `NormalizedPath`'s `Deref` autoderef instead of its inherent normalized methods.
- `ban_dashmap_guard_across_blocking`: bans holding a `DashMap::get` guard across awaits, filesystem/process work, or a mutation of the same map.
- `ban_discarded_write_result`: bans discarding the `Result` of a write-ish call (`let _ = …` / statement-position `.ok();`) in the daemon's persistence modules (#1163 / #1177).
- `enforce_platform_boundary`: confines host-platform cfg/native APIs to the `zccache-platform` leaf crate, pre-expansion, with a ratcheting exact-occurrence baseline (#1365 / #1366).

All nine libraries use the published Dylint 6.0.1 crates and the pinned
`nightly-2026-05-26` toolchain. The supported `cargo-dylint` driver is used
directly; no custom driver checkout or library alias repair is required.

Run from the repository root:

```bash
cargo dylint --all --workspace
```

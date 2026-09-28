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
- `enforce_platform_boundary`: confines host-platform cfg/native APIs to approved product adapters over kernal-api, pre-expansion, with a ratcheting exact-occurrence baseline (#1365 / #1366).
- `ban_registered_env_read`: confines environment access to registered, auditable call sites.

All ten libraries use Dylint 6.0.3 and the pinned `nightly-2026-05-28`
toolchain. Soldr downloads the published tools and driver. Each crate uses
`dylint-link` to emit the toolchain-suffixed library Dylint expects.

Run from the repository root:

```bash
env -u RUSTUP_TOOLCHAIN -u RUSTFLAGS SOLDR_DYLINT_TOOLCHAIN=nightly-2026-05-28 soldr dylint prepare
./lint
```

Full `./lint` runs formatting, Clippy, workspace library/binary Dylint, and docs checks.
`./lint <file.rs>` checks formatting and Clippy for that file's crate, but not
Dylint; `./lint --fix` formats and runs Clippy, but does not run Dylint.

Late lints only see cfg-selected code, so a host-only pass misses the other
operating systems' modules (#1740). `./lint` and `./lint --dylint-only` (the
CI `Dylint` job) run the host pass, then a cross-target `cargo check` pass for
each other OS in `ci/lint.py` `DYLINT_OS_TARGETS` (Linux GNU x64, Windows MSVC
x64, macOS arm64); `soldr rustup target add` installs each target's std on the
pinned nightly. One triple per OS covers all eight published triples because
`ci/check_dylint_wiring.py` rejects first-party `cfg` gates on anything but
`unix`, `windows`, `target_family`, and those three `target_os` values.
`ci/dylint-target-fixture` proves each OS leg reports a planted finding.

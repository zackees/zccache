//! Compilation context and cache key computation.
//!
//! The context key identifies a unique (source + flags) combination
//! and maps to an include list. The artifact key incorporates content
//! hashes of all files for artifact store lookup.
//!
//! Split into focused submodules so each file stays under 1,000 LOC:
//! - this file: type definitions, key computation, path-normalization helpers,
//!   and the `VOLATILE_CARGO_ENV_VARS` allow-list.
//! - `tests` (cfg(test) only): split per surface — `cc` (C/C++ tests) and
//!   `rustc` (rustc tests).

use std::path::Path;
use std::sync::Arc;

use crate::core::path::normalize_for_key;
use crate::core::NormalizedPath;
use crate::hash::ContentHash;

use super::args::ParsedArgs;
use super::rustc_args::RustcParsedArgs;
use super::search_paths::IncludeSearchPaths;

#[cfg(test)]
mod tests;

/// blake3 hash identifying a (source + include_dirs + defines + flags) combination.
/// Same context key = same set of resolved headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContextKey(ContentHash);

impl ContextKey {
    /// Returns the underlying hash.
    #[must_use]
    pub fn hash(&self) -> &ContentHash {
        &self.0
    }

    /// Construct from raw 32-byte hash (for deserialization).
    #[must_use]
    pub fn from_raw(bytes: [u8; 32]) -> Self {
        Self(ContentHash::from_bytes(bytes))
    }
}

impl std::fmt::Display for ContextKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ctx:{}", self.0.to_hex())
    }
}

/// blake3 hash identifying a specific compilation output.
/// Same artifact key = the exact same `.o` file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArtifactKey(ContentHash);

impl ArtifactKey {
    /// Returns the underlying hash.
    #[must_use]
    pub fn hash(&self) -> &ContentHash {
        &self.0
    }

    /// Construct from raw 32-byte hash (for deserialization).
    #[must_use]
    pub fn from_raw(bytes: [u8; 32]) -> Self {
        Self(ContentHash::from_bytes(bytes))
    }
}

impl std::fmt::Display for ArtifactKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "art:{}", self.0.to_hex())
    }
}

/// All inputs defining a compilation context.
#[derive(Debug, Clone)]
pub struct CompileContext {
    /// Absolute path to the source file.
    pub source_file: NormalizedPath,
    /// Ordered include search paths.
    pub include_search: IncludeSearchPaths,
    /// Sorted defines (-D flags).
    pub defines: Vec<String>,
    /// Sorted cache-relevant flags (-std, -O, -f, etc.).
    pub flags: Vec<String>,
    /// Force-included files (-include).
    pub force_includes: Vec<NormalizedPath>,
    /// Sorted unknown flags — not recognized by the parser but still
    /// affect compilation output, so they must be part of the cache key.
    pub unknown_flags: Vec<String>,
}

impl CompileContext {
    /// Build a `CompileContext` from parsed arguments (consumes the args to avoid cloning).
    #[must_use]
    pub fn from_parsed_args(args: ParsedArgs) -> Self {
        let mut defines = args.defines;
        defines.sort();
        let mut flags = args.flags;
        flags.sort();
        let mut unknown_flags = args.unknown_flags;
        unknown_flags.sort();

        Self {
            source_file: args.source_file,
            include_search: args.include_search,
            defines,
            flags,
            force_includes: args.force_includes,
            unknown_flags,
        }
    }

    /// Compute the context key.
    ///
    /// Includes: source file path, include dirs (in order), sorted defines,
    /// sorted flags, unknown flags, force includes. Passes `None` for both
    /// `key_root` and `worktree_salt` — callers that need either should call
    /// [`compute_context_key`] directly.
    #[must_use]
    pub fn context_key(&self) -> ContextKey {
        compute_context_key(self, None, None)
    }
}

/// Reduce an `--extern name=path` value to its identity-bearing tail.
///
/// Cargo embeds a per-package `metadata=` hash in the file name (e.g.
/// `libserde-abc123.rmeta`), so the file name alone uniquely identifies the
/// extern. The directory prefix is incidental (changes per workspace
/// location, target dir, profile dir layout) and must NOT enter the cache key.
///
/// If the path has no file-name component (defensively — shouldn't happen for
/// real `--extern` values), fall back to the full string so we still hash
/// _something_ stable rather than silently collapsing distinct externs.
fn extern_path_key(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
}

pub fn normalize_key_path(path: &Path, key_root: Option<&Path>) -> String {
    if let Some(root) = key_root {
        if let Ok(stripped) = path.strip_prefix(root) {
            return normalize_for_key(stripped);
        }
    }

    normalize_for_key(path)
}

fn normalize_remap_path_prefix_for_key(remap: &str, key_root: Option<&Path>) -> String {
    let Some(root) = key_root else {
        return remap.to_string();
    };
    let Some((from, to)) = remap.split_once('=') else {
        return remap.to_string();
    };

    let from_path = Path::new(from);
    if from_path.strip_prefix(root).is_ok() {
        format!("{}={}", normalize_key_path(from_path, key_root), to)
    } else {
        remap.to_string()
    }
}

fn normalize_cxx_prefix_map_flag_for_key(flag: &str, key_root: Option<&Path>) -> String {
    const PREFIX_MAP_FLAGS: [&str; 5] = [
        "-ffile-prefix-map=",
        "-fdebug-prefix-map=",
        "-fmacro-prefix-map=",
        "-fcoverage-prefix-map=",
        "-fprofile-prefix-map=",
    ];

    for prefix in PREFIX_MAP_FLAGS {
        if let Some(remap) = flag.strip_prefix(prefix) {
            return format!(
                "{}{}",
                prefix,
                normalize_remap_path_prefix_for_key(remap, key_root)
            );
        }
    }

    flag.to_string()
}

/// Compute the context key for a C/C++ compilation context.
///
/// When `key_root` is provided, paths under that root are hashed relative to it
/// so equivalent workspaces can share cache keys across root-directory renames.
///
/// When `worktree_salt` is provided, its byte representation is folded into the
/// hash so the resulting key is unique to that worktree. This is the
/// correctness escape hatch for compile modes whose artifacts the compiler
/// embeds absolute paths inside in a form the `-ffile-prefix-map` family of
/// flags can't scrub:
///
/// * PCH builds (`-x c++-header` / `-x c-header`) — the `.pch`/`.gch` binary
///   serialises the AST's header-path table.
/// * MSVC compiles — `cl.exe` has no `-fmacro-prefix-map` equivalent.
///
/// See `crate::daemon::server::keys::requires_worktree_in_key` for the
/// truth table and issue #474 for the cross-clone leak this guards against.
/// All other callers (rustc, clang/gcc non-PCH) pass `None` and continue to
/// share cache entries across worktrees of the same commit.
#[must_use]
pub fn compute_context_key(
    ctx: &CompileContext,
    key_root: Option<&Path>,
    worktree_salt: Option<&Path>,
) -> ContextKey {
    compute_context_key_with(ctx, key_root, worktree_salt, |path, root| {
        normalize_key_path(path, root).into()
    })
}

/// Sibling of [`compute_context_key`] that accepts an injectable path
/// normalizer. Issue #561 — lets `DepGraph::register_context` thread its
/// `path_key_cache` (added by #553) through every `normalize_key_path`
/// call, amortizing the per-compile ~50 String allocations across
/// sequential compiles that share the same include / force-include set
/// (the cpp-inline Single-file Cold benchmark's 50 sequential
/// invocations are the dominant beneficiary).
///
/// The default `compute_context_key` delegates with
/// `|p, r| normalize_key_path(p, r).into()` so callers without a
/// `DepGraph` are unaffected.
#[must_use]
pub fn compute_context_key_with<F>(
    ctx: &CompileContext,
    key_root: Option<&Path>,
    worktree_salt: Option<&Path>,
    mut normalize: F,
) -> ContextKey
where
    F: FnMut(&Path, Option<&Path>) -> Arc<str>,
{
    let mut hasher = blake3::Hasher::new();

    hasher.update(b"zccache-context-key-v1\0");

    if let Some(salt) = worktree_salt {
        // Domain-tagged so the salt can't collide with any future hash
        // input that happens to start with the same bytes. `None` is the
        // common case and writes no bytes — keys produced with no salt
        // are byte-identical to pre-#474 keys.
        hasher.update(b"worktree-salt\0");
        hasher.update(crate::core::path::normalize_for_key(salt).as_bytes());
        hasher.update(b"\0");
    }

    hasher.update(normalize(ctx.source_file.as_ref(), key_root).as_bytes());
    hasher.update(b"\0");

    hasher.update(b"iquote\0");
    for dir in &ctx.include_search.iquote {
        hasher.update(normalize(dir.as_ref(), key_root).as_bytes());
        hasher.update(b"\0");
    }

    hasher.update(b"user\0");
    for dir in &ctx.include_search.user {
        hasher.update(normalize(dir.as_ref(), key_root).as_bytes());
        hasher.update(b"\0");
    }

    hasher.update(b"system\0");
    for dir in &ctx.include_search.system {
        hasher.update(normalize(dir.as_ref(), key_root).as_bytes());
        hasher.update(b"\0");
    }

    hasher.update(b"after\0");
    for dir in &ctx.include_search.after {
        hasher.update(normalize(dir.as_ref(), key_root).as_bytes());
        hasher.update(b"\0");
    }

    hasher.update(b"defines\0");
    for def in &ctx.defines {
        hasher.update(def.as_bytes());
        hasher.update(b"\0");
    }

    hasher.update(b"flags\0");
    for flag in &ctx.flags {
        let flag = normalize_cxx_prefix_map_flag_for_key(flag, key_root);
        hasher.update(flag.as_bytes());
        hasher.update(b"\0");
    }

    hasher.update(b"force-include\0");
    for fi in &ctx.force_includes {
        hasher.update(normalize(fi.as_ref(), key_root).as_bytes());
        hasher.update(b"\0");
    }

    hasher.update(b"unknown\0");
    for flag in &ctx.unknown_flags {
        let flag = normalize_cxx_prefix_map_flag_for_key(flag, key_root);
        hasher.update(flag.as_bytes());
        hasher.update(b"\0");
    }

    ContextKey(ContentHash::from_bytes(*hasher.finalize().as_bytes()))
}

/// Compute the artifact key from a context key and file content hashes.
///
/// The artifact key uniquely identifies a specific compilation output.
/// `file_hashes` should contain `(path, content_hash)` pairs for the
/// source file and all resolved headers, sorted by path.
pub fn compute_artifact_key<P: AsRef<Path> + Ord>(
    context_key: &ContextKey,
    file_hashes: &mut [(P, ContentHash)],
    key_root: Option<&Path>,
) -> ArtifactKey {
    compute_artifact_key_with(context_key, file_hashes, key_root, |path, key_root| {
        normalize_key_path(path, key_root).into()
    })
}

/// Like [`compute_artifact_key`] but the per-header path normalization
/// is delegated to a caller-supplied closure. The daemon plumbs in a
/// closure that consults `DepGraph::cached_normalize_key_path` so the
/// per-compile allocations amortize across the daemon's lifetime
/// (issue #550). The closure is `FnMut` only because the impl forwards
/// it through the iterator; in practice it's a `Fn`-shaped lookup.
/// Fast-path variant of [`compute_artifact_key_with`] for the common case
/// where the caller already holds owned [`NormalizedPath`] values and has
/// no `key_root` (the cc/cpp compile path without `-ffile-prefix-map`).
///
/// Issue #585: post-#576 each [`NormalizedPath`] caches its
/// `normalize_for_key` result in the struct's `key` field. With no
/// `key_root`, that cached key IS the bytes we want to hash — no
/// allocation, no DashMap lookup, no closure call. The previous shape
/// went through `cached_normalize_key_path` which allocated 4 owned
/// objects per lookup just to construct the `DashMap` key.
///
/// Output is bit-identical to `compute_artifact_key_with` when called
/// with `key_root: None` and a closure that returns
/// `normalize_for_key(path).into()`.
#[must_use]
pub fn compute_artifact_key_normalized_inplace(
    context_key: &ContextKey,
    file_hashes: &mut [(crate::core::NormalizedPath, ContentHash)],
) -> ArtifactKey {
    compute_artifact_key_normalized_with_root(context_key, file_hashes, None)
}

/// Issue #591: extension of [`compute_artifact_key_normalized_inplace`]
/// that also handles `key_root: Some`. For paths NOT under `key_root`
/// (the common case for system headers), the path-key bytes are just
/// `NormalizedPath::case_key()` — no allocation. For paths under
/// `key_root`, we fall back to `normalize_key_path(path, Some(root))`.
///
/// Replaces the closure-based slow path through
/// `compute_artifact_key_with` + `cached_normalize_key_path` which
/// allocated 1 String per entry even after #590's cache bypass.
#[must_use]
pub fn compute_artifact_key_normalized_with_root(
    context_key: &ContextKey,
    file_hashes: &[(crate::core::NormalizedPath, ContentHash)],
    key_root: Option<&Path>,
) -> ArtifactKey {
    use std::borrow::Cow;

    // Materialize path-keys: borrow from NormalizedPath::key for paths
    // not under key_root (zero alloc); compute fresh for project-local.
    let mut indexed: Vec<(Cow<'_, str>, ContentHash)> = file_hashes
        .iter()
        .map(|(np, h)| {
            let path_key: Cow<'_, str> = match key_root {
                Some(root) if np.as_path().starts_with(root) => {
                    Cow::Owned(normalize_key_path(np.as_path(), Some(root)))
                }
                _ => Cow::Borrowed(
                    np.case_key()
                        .expect("NormalizedPath::key is always populated post-#576"),
                ),
            };
            (path_key, *h)
        })
        .collect();
    indexed.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"zccache-artifact-key-v1\0");
    hasher.update(context_key.0.as_bytes());
    hasher.update(b"\0");

    for (path_key, hash) in &indexed {
        hasher.update(path_key.as_bytes());
        hasher.update(b"\0");
        hasher.update(hash.as_bytes());
        hasher.update(b"\0");
    }

    ArtifactKey(ContentHash::from_bytes(*hasher.finalize().as_bytes()))
}

pub fn compute_artifact_key_with<P, F>(
    context_key: &ContextKey,
    file_hashes: &mut [(P, ContentHash)],
    key_root: Option<&Path>,
    mut normalize: F,
) -> ArtifactKey
where
    P: AsRef<Path> + Ord,
    F: FnMut(&Path, Option<&Path>) -> Arc<str>,
{
    // Issue #571: pre-normalize each path once (O(n) via the cached
    // closure), then sort by the cheap Arc<str> key (O(n log n) byte
    // compares). The prior path called `NormalizedPath::cmp` inside
    // `sort_by`, which invoked `normalize_for_key` on BOTH operands of
    // every comparison — O(n log n) normalizations bypassed the
    // #553 cache entirely. With ~600 transitive headers per cpp
    // compile, that was ~10k normalize_for_key calls per miss; this
    // collapses to ~600 calls (most hit the cache after the first
    // compile in a session) plus cheap byte compares.
    //
    // Hash output is bit-identical to the prior path: the sort order
    // is determined by the same normalized path-keys, and the blake3
    // input bytes (path-key, separator, content-hash, separator) are
    // emitted in the same order.
    let mut indexed: Vec<(Arc<str>, ContentHash)> = file_hashes
        .iter()
        .map(|(p, h)| (normalize(p.as_ref(), key_root), *h))
        .collect();
    indexed.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"zccache-artifact-key-v1\0");
    hasher.update(context_key.0.as_bytes());
    hasher.update(b"\0");

    for (path_key, hash) in &indexed {
        hasher.update(path_key.as_bytes());
        hasher.update(b"\0");
        hasher.update(hash.as_bytes());
        hasher.update(b"\0");
    }

    ArtifactKey(ContentHash::from_bytes(*hasher.finalize().as_bytes()))
}

/// CARGO_* environment variables that must NOT participate in the cache key.
///
/// These are volatile (absolute paths or build-host transients) and either do
/// not affect compiled output or affect it only via paths that should already
/// be normalized elsewhere. Including them cascades cache invalidation across
/// the entire dep graph whenever the workspace is moved, cloned, or re-checked
/// out at a different on-disk location.
///
/// What stays in the key (everything else starting with `CARGO_`):
/// - `CARGO_PKG_VERSION`, `CARGO_PKG_NAME`, `CARGO_PKG_AUTHORS`,
///   `CARGO_PKG_DESCRIPTION`, `CARGO_PKG_HOMEPAGE`, `CARGO_PKG_REPOSITORY`,
///   `CARGO_PKG_LICENSE`, `CARGO_PKG_RUST_VERSION`, `CARGO_CRATE_NAME`, etc.
///   These feed `env!()` macros and are baked into the compiled artifact.
///
/// Already excluded earlier in the filter (orthogonal reasons):
/// - `CARGO_MAKEFLAGS` (job-server token, transient).
/// - `CARGO_INCREMENTAL` (handled by stripping `-C incremental` from args).
///
/// Filtered here (this list):
/// - `CARGO_MANIFEST_DIR` — absolute path to the crate dir; changes per
///   checkout location. Cascades the cache.
/// - `CARGO_MANIFEST_PATH` — absolute path to `Cargo.toml`; same issue.
/// - `CARGO_TARGET_DIR` — output-placement state set by cargo. Two worktrees
///   that share a zccache cache but pick different relative target-dir leaf
///   names (e.g. `parent-cache-main-target` vs `parent-cache-sub-target`)
///   otherwise cold-miss every rustc compilation even with
///   `ZCCACHE_PATH_REMAP=auto`. Filtering is sound because `CARGO_TARGET_DIR`
///   only directs cargo where to place build output — it is not embedded in
///   rustc output via `env!()` in normal builds, and `--out-dir` / `-L` /
///   `--extern` directory prefixes that cargo derives from it are already
///   non-cache-key state (out_dir excluded; search_paths excluded; extern
///   paths reduced to file-name identity). See issue #396.
const VOLATILE_CARGO_ENV_VARS: &[&str] = &[
    "CARGO_MANIFEST_DIR",
    "CARGO_MANIFEST_PATH",
    "CARGO_TARGET_DIR",
];

/// All inputs defining a rustc compilation context.
///
/// Separate from `CompileContext` because Rust's compilation model differs
/// fundamentally from C/C++: no include paths, `--cfg` instead of `-D`,
/// `--extern` crates instead of headers, etc.
#[derive(Debug, Clone)]
pub struct RustcCompileContext {
    /// Absolute path to the source file.
    pub source_file: NormalizedPath,
    /// `--crate-name` value.
    pub crate_name: Option<String>,
    /// Sorted `--crate-type` values.
    pub crate_types: Vec<String>,
    /// `--edition` value.
    pub edition: Option<String>,
    /// Sorted `--emit` types.
    pub emit_types: Vec<String>,
    /// Sorted `--cfg` values.
    pub cfgs: Vec<String>,
    /// Sorted `--check-cfg` values.
    pub check_cfgs: Vec<String>,
    /// Sorted cache-relevant `-C` codegen options.
    pub codegen_flags: Vec<String>,
    /// Cargo's `-C metadata=` disambiguator for this compilation unit.
    pub cargo_metadata: Option<String>,
    /// Cargo's `-C extra-filename=` suffix for output artifact names.
    pub extra_filename: Option<String>,
    /// `--target` triple.
    pub target: Option<String>,
    /// `--cap-lints` value.
    pub cap_lints: Option<String>,
    /// Extern crate `(name, path)` pairs, sorted. Paths included so that
    /// `--extern a=v1.rlib` and `--extern a=v2.rlib` get different context keys.
    pub extern_crates: Vec<(String, String)>,
    /// Sorted lint flags (`-A`, `-W`, `-D`, `-F`).
    pub lint_flags: Vec<String>,
    /// Sorted unknown flags.
    pub unknown_flags: Vec<String>,
    /// Sorted `--remap-path-prefix` values (affect embedded paths in output).
    pub remap_path_prefixes: Vec<String>,
    /// Sorted CARGO_* environment variables that affect compilation via `env!()`.
    pub env_vars: Vec<(String, String)>,
    /// Hash of the compiler binary (different rustc versions produce different output).
    pub compiler_hash: Option<ContentHash>,
}

impl RustcCompileContext {
    /// Build from parsed rustc args and client environment.
    ///
    /// `client_env` should be the CARGO_* env vars from the client process.
    /// These affect compilation via `env!()` macros and must be in the cache key.
    #[must_use]
    pub fn from_parsed_args(
        args: &RustcParsedArgs,
        client_env: &[(String, String)],
        compiler_hash: Option<ContentHash>,
    ) -> Self {
        let mut crate_types = args.crate_types.clone();
        crate_types.sort();
        let mut emit_types = args.emit_types.clone();
        emit_types.sort();
        let mut extern_crates: Vec<(String, String)> = args
            .externs
            .iter()
            .map(|e| (e.name.clone(), e.path.to_string_lossy().into_owned()))
            .collect();
        extern_crates.sort();
        let mut remap_path_prefixes = args.remap_path_prefixes.clone();
        remap_path_prefixes.sort();

        // Filter CARGO_* env vars — these affect compilation output via env!() macro.
        // Exclude CARGO_MAKEFLAGS (job server, not output-affecting),
        // CARGO_INCREMENTAL (handled by stripping -C incremental), and
        // VOLATILE_CARGO_ENV_VARS (absolute paths that cascade cache misses).
        let mut env_vars: Vec<(String, String)> = client_env
            .iter()
            .filter(|(k, _)| {
                k.starts_with("CARGO_")
                    && k != "CARGO_MAKEFLAGS"
                    && k != "CARGO_INCREMENTAL"
                    && !VOLATILE_CARGO_ENV_VARS.contains(&k.as_str())
            })
            .cloned()
            .collect();
        env_vars.sort();

        Self {
            source_file: args.source_file.clone(),
            crate_name: args.crate_name.clone(),
            crate_types,
            edition: args.edition.clone(),
            emit_types,
            cfgs: args.cfgs.clone(),
            check_cfgs: args.check_cfgs.clone(),
            codegen_flags: args.codegen_flags.clone(),
            cargo_metadata: args.cargo_metadata.clone(),
            extra_filename: args.extra_filename.clone(),
            target: args.target.clone(),
            cap_lints: args.cap_lints.clone(),
            extern_crates,
            lint_flags: args.lint_flags.clone(),
            unknown_flags: args.unknown_flags.clone(),
            remap_path_prefixes,
            env_vars,
            compiler_hash,
        }
    }

    /// Compute the context key.
    ///
    /// Uses a different domain tag from C/C++ to avoid collisions.
    #[must_use]
    pub fn context_key(&self) -> ContextKey {
        self.context_key_with_root(None)
    }

    /// Compute the context key, optionally normalizing project-local paths.
    ///
    /// When `key_root` is provided, source paths and safe path-bearing key
    /// fields under that root are hashed relative to it so equivalent
    /// workspaces can share cache keys across root-directory renames.
    #[must_use]
    pub fn context_key_with_root(&self, key_root: Option<&Path>) -> ContextKey {
        let mut hasher = blake3::Hasher::new();

        hasher.update(b"zccache-rustc-context-key-v2\0");

        // Compiler binary hash (different rustc versions -> different output).
        if let Some(ref ch) = self.compiler_hash {
            hasher.update(b"compiler\0");
            hasher.update(ch.as_bytes());
            hasher.update(b"\0");
        }

        // Source file.
        let source_file = normalize_key_path(&self.source_file, key_root);
        hasher.update(source_file.as_bytes());
        hasher.update(b"\0");

        // Crate name.
        if let Some(ref name) = self.crate_name {
            hasher.update(b"crate-name\0");
            hasher.update(name.as_bytes());
            hasher.update(b"\0");
        }

        // Crate types (sorted).
        hasher.update(b"crate-types\0");
        for ct in &self.crate_types {
            hasher.update(ct.as_bytes());
            hasher.update(b"\0");
        }

        // Edition.
        if let Some(ref edition) = self.edition {
            hasher.update(b"edition\0");
            hasher.update(edition.as_bytes());
            hasher.update(b"\0");
        }

        // Emit types (sorted).
        hasher.update(b"emit\0");
        for et in &self.emit_types {
            hasher.update(et.as_bytes());
            hasher.update(b"\0");
        }

        // Cfg values (sorted).
        hasher.update(b"cfg\0");
        for cfg in &self.cfgs {
            hasher.update(cfg.as_bytes());
            hasher.update(b"\0");
        }

        // Check-cfg values (sorted).
        hasher.update(b"check-cfg\0");
        for cfg in &self.check_cfgs {
            hasher.update(cfg.as_bytes());
            hasher.update(b"\0");
        }

        // Codegen flags (sorted).
        hasher.update(b"codegen\0");
        for flag in &self.codegen_flags {
            hasher.update(flag.as_bytes());
            hasher.update(b"\0");
        }

        if let Some(ref metadata) = self.cargo_metadata {
            hasher.update(b"cargo-metadata\0");
            hasher.update(metadata.as_bytes());
            hasher.update(b"\0");
        }

        if let Some(ref extra_filename) = self.extra_filename {
            hasher.update(b"extra-filename\0");
            hasher.update(extra_filename.as_bytes());
            hasher.update(b"\0");
        }

        // Target.
        if let Some(ref target) = self.target {
            hasher.update(b"target\0");
            hasher.update(target.as_bytes());
            hasher.update(b"\0");
        }

        // Cap lints.
        if let Some(ref cap) = self.cap_lints {
            hasher.update(b"cap-lints\0");
            hasher.update(cap.as_bytes());
            hasher.update(b"\0");
        }

        // Extern crate (name, path) pairs - hash only the file name component,
        // not the absolute directory prefix. The file name carries cargo's
        // per-package `metadata=` hash (e.g. `libserde-abc123.rmeta`), which
        // uniquely identifies the extern's identity. Including the directory
        // prefix would cascade cache misses across workspace clones / renames
        // (issue #139, fix #1). Different `--extern a=v1.rmeta` vs
        // `--extern a=v2.rmeta` still get different keys because the metadata
        // suffix is part of the file name.
        hasher.update(b"externs\0");
        for (name, path) in &self.extern_crates {
            hasher.update(name.as_bytes());
            hasher.update(b"=");
            hasher.update(extern_path_key(path).as_bytes());
            hasher.update(b"\0");
        }

        // Lint flags (sorted).
        hasher.update(b"lints\0");
        for flag in &self.lint_flags {
            hasher.update(flag.as_bytes());
            hasher.update(b"\0");
        }

        // Unknown flags (sorted).
        hasher.update(b"unknown\0");
        for flag in &self.unknown_flags {
            hasher.update(flag.as_bytes());
            hasher.update(b"\0");
        }

        // --remap-path-prefix values (sorted, affect embedded paths in output).
        hasher.update(b"remap\0");
        if key_root.is_some() {
            let mut remap_path_prefixes: Vec<String> = self
                .remap_path_prefixes
                .iter()
                .map(|remap| normalize_remap_path_prefix_for_key(remap, key_root))
                .collect();
            remap_path_prefixes.sort();
            for remap in &remap_path_prefixes {
                hasher.update(remap.as_bytes());
                hasher.update(b"\0");
            }
        } else {
            for remap in &self.remap_path_prefixes {
                hasher.update(remap.as_bytes());
                hasher.update(b"\0");
            }
        }

        // CARGO_* environment variables (sorted, affect env!() macro output).
        //
        // Defense-in-depth: we ALSO filter VOLATILE_CARGO_ENV_VARS here, not
        // only in `from_parsed_args`. The struct is public and may be built
        // directly (in tests or by future call sites). Hashing must be the
        // single source of truth on what counts. See `VOLATILE_CARGO_ENV_VARS`
        // for the rationale (issue #139).
        hasher.update(b"env\0");
        for (key, val) in &self.env_vars {
            if VOLATILE_CARGO_ENV_VARS.contains(&key.as_str()) {
                continue;
            }
            hasher.update(key.as_bytes());
            hasher.update(b"=");
            hasher.update(val.as_bytes());
            hasher.update(b"\0");
        }

        ContextKey(ContentHash::from_bytes(*hasher.finalize().as_bytes()))
    }

    /// Compatibility key for reusing build-mode rustc metadata during check.
    ///
    /// This is deliberately narrower than the normal rustc context key. It is
    /// only produced for Cargo's check-style metadata emits and the matching
    /// build-style metadata+link emits:
    ///
    /// - `metadata` <-> `metadata,link`
    /// - `dep-info,metadata` <-> `dep-info,metadata,link`
    ///
    /// Cargo gives check and build units different `-C metadata` /
    /// `-C extra-filename` values, so those output-placement fields are not
    /// part of this alias. Correctness is guarded later by source/dependency
    /// content hashes and by comparing current extern content hashes with the
    /// candidate build entry's extern content hashes.
    #[must_use]
    pub fn check_metadata_compat_key_with_root(
        &self,
        key_root: Option<&Path>,
    ) -> Option<ContextKey> {
        let normalized_emit = normalized_check_metadata_emit(&self.emit_types)?;
        if self.crate_types.iter().any(|ct| {
            matches!(
                ct.as_str(),
                "bin" | "proc-macro" | "staticlib" | "dylib" | "cdylib"
            )
        }) {
            return None;
        }

        let mut hasher = blake3::Hasher::new();
        hasher.update(b"zccache-rustc-check-metadata-compat-key-v1\0");

        if let Some(ref ch) = self.compiler_hash {
            hasher.update(b"compiler\0");
            hasher.update(ch.as_bytes());
            hasher.update(b"\0");
        }

        let source_file = normalize_key_path(&self.source_file, key_root);
        hasher.update(source_file.as_bytes());
        hasher.update(b"\0");

        if let Some(ref name) = self.crate_name {
            hasher.update(b"crate-name\0");
            hasher.update(name.as_bytes());
            hasher.update(b"\0");
        }

        hasher.update(b"crate-types\0");
        for ct in &self.crate_types {
            hasher.update(ct.as_bytes());
            hasher.update(b"\0");
        }

        if let Some(ref edition) = self.edition {
            hasher.update(b"edition\0");
            hasher.update(edition.as_bytes());
            hasher.update(b"\0");
        }

        hasher.update(b"emit\0");
        for et in normalized_emit {
            hasher.update(et.as_bytes());
            hasher.update(b"\0");
        }

        hasher.update(b"cfg\0");
        for cfg in &self.cfgs {
            hasher.update(cfg.as_bytes());
            hasher.update(b"\0");
        }

        hasher.update(b"check-cfg\0");
        for cfg in &self.check_cfgs {
            hasher.update(cfg.as_bytes());
            hasher.update(b"\0");
        }

        hasher.update(b"codegen\0");
        for flag in &self.codegen_flags {
            hasher.update(flag.as_bytes());
            hasher.update(b"\0");
        }

        if let Some(ref target) = self.target {
            hasher.update(b"target\0");
            hasher.update(target.as_bytes());
            hasher.update(b"\0");
        }

        if let Some(ref cap) = self.cap_lints {
            hasher.update(b"cap-lints\0");
            hasher.update(cap.as_bytes());
            hasher.update(b"\0");
        }

        // Extern paths carry Cargo's check/build-specific filename suffixes.
        // The compatibility lookup compares extern content hashes by crate
        // name, so the alias key only needs the names to preserve dependency
        // shape without baking in the output suffix.
        hasher.update(b"externs\0");
        for (name, _) in &self.extern_crates {
            hasher.update(name.as_bytes());
            hasher.update(b"\0");
        }

        hasher.update(b"lints\0");
        for flag in &self.lint_flags {
            hasher.update(flag.as_bytes());
            hasher.update(b"\0");
        }

        hasher.update(b"unknown\0");
        for flag in &self.unknown_flags {
            hasher.update(flag.as_bytes());
            hasher.update(b"\0");
        }

        hasher.update(b"remap\0");
        if key_root.is_some() {
            let mut remap_path_prefixes: Vec<String> = self
                .remap_path_prefixes
                .iter()
                .map(|remap| normalize_remap_path_prefix_for_key(remap, key_root))
                .collect();
            remap_path_prefixes.sort();
            for remap in &remap_path_prefixes {
                hasher.update(remap.as_bytes());
                hasher.update(b"\0");
            }
        } else {
            for remap in &self.remap_path_prefixes {
                hasher.update(remap.as_bytes());
                hasher.update(b"\0");
            }
        }

        hasher.update(b"env\0");
        for (key, val) in &self.env_vars {
            if VOLATILE_CARGO_ENV_VARS.contains(&key.as_str()) {
                continue;
            }
            hasher.update(key.as_bytes());
            hasher.update(b"=");
            hasher.update(val.as_bytes());
            hasher.update(b"\0");
        }

        Some(ContextKey(ContentHash::from_bytes(
            *hasher.finalize().as_bytes(),
        )))
    }
}

fn normalized_check_metadata_emit(emit_types: &[String]) -> Option<&'static [&'static str]> {
    let has = |needle: &str| emit_types.iter().any(|emit| emit == needle);
    match emit_types.len() {
        1 if has("metadata") => Some(&["metadata"]),
        2 if has("metadata") && has("link") => Some(&["metadata"]),
        2 if has("dep-info") && has("metadata") => Some(&["dep-info", "metadata"]),
        3 if has("dep-info") && has("metadata") && has("link") => Some(&["dep-info", "metadata"]),
        _ => None,
    }
}

/// Compute the artifact key for a rustc compilation.
///
/// Like `compute_artifact_key` for C/C++, but also incorporates
/// extern crate content hashes (analogous to header content hashes).
pub fn compute_rustc_artifact_key<P: AsRef<Path> + Ord>(
    context_key: &ContextKey,
    file_hashes: &mut [(P, ContentHash)],
    extern_hashes: &mut [(String, ContentHash)],
) -> ArtifactKey {
    compute_rustc_artifact_key_with_root(context_key, file_hashes, extern_hashes, None)
}

/// Compute the rustc artifact key, optionally normalizing project-local files.
///
/// When `key_root` is provided, source and dependency file paths under that
/// root are hashed relative to it. Extern hashes remain keyed by crate name.
pub fn compute_rustc_artifact_key_with_root<P: AsRef<Path> + Ord>(
    context_key: &ContextKey,
    file_hashes: &mut [(P, ContentHash)],
    extern_hashes: &mut [(String, ContentHash)],
    key_root: Option<&Path>,
) -> ArtifactKey {
    compute_rustc_artifact_key_with_root_with(
        context_key,
        file_hashes,
        extern_hashes,
        key_root,
        |path, key_root| normalize_key_path(path, key_root).into(),
    )
}

/// Like [`compute_rustc_artifact_key_with_root`] but the per-header path
/// normalization is delegated to a caller-supplied closure. Used by the
/// daemon's rustc miss/update paths to consult
/// `DepGraph::cached_normalize_key_path` for the per-header allocation
/// amortization (issue #550).
pub fn compute_rustc_artifact_key_with_root_with<P, F>(
    context_key: &ContextKey,
    file_hashes: &mut [(P, ContentHash)],
    extern_hashes: &mut [(String, ContentHash)],
    key_root: Option<&Path>,
    mut normalize: F,
) -> ArtifactKey
where
    P: AsRef<Path> + Ord,
    F: FnMut(&Path, Option<&Path>) -> Arc<str>,
{
    // Issue #571: pre-normalize each path once (O(n) via the cached
    // closure), then sort on the cached Arc<str> keys (O(n log n) byte
    // compares). The previous shape called `normalize` twice per
    // sort-comparison AND once per hash-loop entry — 3 calls per
    // element. With ~600 transitive headers per cpp/rust compile,
    // this collapses ~10k normalize calls into ~600. Hash output is
    // bit-identical: same sort order, same blake3 input bytes.
    let mut indexed: Vec<(Arc<str>, ContentHash)> = file_hashes
        .iter()
        .map(|(p, h)| (normalize(p.as_ref(), key_root), *h))
        .collect();
    indexed.sort_by(|a, b| a.0.cmp(&b.0));
    extern_hashes.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"zccache-rustc-artifact-key-v1\0");
    hasher.update(context_key.0.as_bytes());
    hasher.update(b"\0");

    // Source + dependency file hashes.
    for (path_key, hash) in &indexed {
        hasher.update(path_key.as_bytes());
        hasher.update(b"\0");
        hasher.update(hash.as_bytes());
        hasher.update(b"\0");
    }

    // Extern crate content hashes.
    hasher.update(b"externs\0");
    for (name, hash) in extern_hashes.iter() {
        hasher.update(name.as_bytes());
        hasher.update(b"\0");
        hasher.update(hash.as_bytes());
        hasher.update(b"\0");
    }

    ArtifactKey(ContentHash::from_bytes(*hasher.finalize().as_bytes()))
}

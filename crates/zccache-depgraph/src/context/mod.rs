//! Compilation context and cache key computation.
//!
//! The context key identifies a unique (source + flags) combination
//! and maps to an include list. The artifact key incorporates content
//! hashes of all files for artifact store lookup.
//!
//! Split into focused submodules so each file stays under 1,000 LOC:
//! - this file: type definitions, context-key computation,
//!   path-normalization helpers, and the `VOLATILE_CARGO_ENV_VARS` allow-list.
//! - `artifact_keys`: generic C/C++ artifact identity.
//! - `rustc_keys`: rustc artifact, verdict, and env-dependency identity.
//! - `tests` (cfg(test) only): split per surface — `cc` (C/C++ tests) and
//!   `rustc` (rustc tests).

use std::path::Path;
use std::sync::Arc;
use zccache_core::path::normalize_for_key;
use zccache_core::NormalizedPath;
use zccache_hash::ContentHash;

use super::args::ParsedArgs;
use super::native_cpu::{host_cpu_identity_salt, is_cxx_native_cpu_flag, is_rustc_native_cpu_flag};
use super::rustc_args::RustcParsedArgs;
use super::search_paths::IncludeSearchPaths;

const DYLINT_CACHE_INPUT_HASH_ENV: &str = "ZCCACHE_DYLINT_CACHE_INPUT_HASH";

mod artifact_keys;
mod rustc_keys;

pub use artifact_keys::{
    compute_artifact_key, compute_artifact_key_normalized_inplace,
    compute_artifact_key_normalized_with_root, compute_artifact_key_with,
};
pub use rustc_keys::{
    compute_rustc_artifact_key, compute_rustc_artifact_key_with_root,
    compute_rustc_artifact_key_with_root_with, compute_rustc_verdict_key,
    fold_rustc_env_deps_into_artifact_key,
};

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
    /// Hash of the compiler binary identity (issue #1166). Non-`Option` by
    /// design: making this field required means "compute a context key
    /// without compiler identity" is unrepresentable at the type level. An
    /// in-place toolchain upgrade (same path, new binary content) must
    /// always change this hash and therefore the resulting context key —
    /// otherwise a stale cache entry can be served for a compiler that no
    /// longer produces the same output.
    pub compiler_hash: ContentHash,
}

impl CompileContext {
    /// Build a `CompileContext` from parsed arguments (consumes the args to avoid cloning).
    #[must_use]
    pub fn from_parsed_args(args: ParsedArgs, compiler_hash: ContentHash) -> Self {
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
            compiler_hash,
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
    compute_context_key_with_native_cpu_salt(ctx, key_root, worktree_salt, None, |path, root| {
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
    normalize: F,
) -> ContextKey
where
    F: FnMut(&Path, Option<&Path>) -> Arc<str>,
{
    compute_context_key_with_native_cpu_salt(ctx, key_root, worktree_salt, None, normalize)
}

/// Variant of [`compute_context_key_with`] with an injectable opaque host-CPU
/// salt for `-march=native`-style invocations.
///
/// Production callers pass `None`, which obtains the current host's stable
/// salt. Tests and embedding applications can supply a synthetic salt to prove
/// cross-host behavior without depending on the CPU that runs the test.
#[must_use]
pub fn compute_context_key_with_native_cpu_salt<F>(
    ctx: &CompileContext,
    key_root: Option<&Path>,
    worktree_salt: Option<&Path>,
    native_cpu_salt: Option<&str>,
    mut normalize: F,
) -> ContextKey
where
    F: FnMut(&Path, Option<&Path>) -> Arc<str>,
{
    let mut hasher = blake3::Hasher::new();

    hasher.update(b"zccache-context-key-v1\0");

    // Compiler binary identity (issue #1166): an in-place toolchain
    // upgrade (same path, new binary content) must change the context
    // key. Unconditional (non-Option field) so this is impossible to omit.
    hasher.update(b"compiler\0");
    hasher.update(ctx.compiler_hash.as_bytes());
    hasher.update(b"\0");

    if ctx
        .flags
        .iter()
        .chain(&ctx.unknown_flags)
        .any(|flag| is_cxx_native_cpu_flag(flag))
    {
        let salt = match native_cpu_salt {
            Some(salt) => salt,
            None => host_cpu_identity_salt(),
        };
        hasher.update(b"native-cpu-host\0");
        hasher.update(salt.as_bytes());
        hasher.update(b"\0");
    }

    if let Some(salt) = worktree_salt {
        // Domain-tagged so the salt can't collide with any future hash
        // input that happens to start with the same bytes. `None` is the
        // common case and writes no bytes — keys produced with no salt
        // are byte-identical to pre-#474 keys.
        hasher.update(b"worktree-salt\0");
        hasher.update(zccache_core::path::normalize_for_key(salt).as_bytes());
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
    /// Cache-relevant `-C` codegen options in command-line order.
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
    /// Hash of the compiler binary (different rustc versions produce
    /// different output). Non-`Option` by design (issue #1166): see the
    /// doc comment on `CompileContext::compiler_hash` for the rationale —
    /// making this required at the type level closes the hole where a
    /// `None` compiler_hash silently omits compiler identity from the key.
    pub compiler_hash: ContentHash,
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
        compiler_hash: ContentHash,
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
        self.context_key_with_root_and_native_cpu_salt(None, None)
    }

    /// Compute the context key, optionally normalizing project-local paths.
    ///
    /// When `key_root` is provided, source paths and safe path-bearing key
    /// fields under that root are hashed relative to it so equivalent
    /// workspaces can share cache keys across root-directory renames.
    #[must_use]
    pub fn context_key_with_root(&self, key_root: Option<&Path>) -> ContextKey {
        self.context_key_with_root_and_native_cpu_salt(key_root, None)
    }

    /// Computes a context key with an injectable opaque native-CPU salt.
    ///
    /// A salt is folded in only for `-C target-cpu=native` (or the defensive
    /// `target-feature=native` spelling), so explicit portable feature lists
    /// retain their ordinary cross-host reuse behavior.
    #[must_use]
    pub fn context_key_with_root_and_native_cpu_salt(
        &self,
        key_root: Option<&Path>,
        native_cpu_salt: Option<&str>,
    ) -> ContextKey {
        let mut hasher = blake3::Hasher::new();

        hasher.update(b"zccache-rustc-context-key-v4\0");

        // Compiler binary hash (different rustc versions -> different
        // output). Unconditional (non-Option field, issue #1166).
        hasher.update(b"compiler\0");
        hasher.update(self.compiler_hash.as_bytes());
        hasher.update(b"\0");

        if self
            .codegen_flags
            .iter()
            .any(|flag| is_rustc_native_cpu_flag(flag))
        {
            let salt = match native_cpu_salt {
                Some(salt) => salt,
                None => host_cpu_identity_salt(),
            };
            hasher.update(b"native-cpu-host\0");
            hasher.update(salt.as_bytes());
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

        // Codegen flags retain command-line order. Repeated options can be
        // additive or last-one-wins, so reordering would risk false hits.
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
            if VOLATILE_CARGO_ENV_VARS.contains(&key.as_str()) || key == DYLINT_CACHE_INPUT_HASH_ENV
            {
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
        self.check_metadata_compat_key_with_root_and_native_cpu_salt(key_root, None)
    }

    /// Native-CPU-salt-injectable variant of
    /// [`Self::check_metadata_compat_key_with_root`].
    #[must_use]
    pub fn check_metadata_compat_key_with_root_and_native_cpu_salt(
        &self,
        key_root: Option<&Path>,
        native_cpu_salt: Option<&str>,
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

        // Unconditional (non-Option field, issue #1166) — see
        // `context_key_with_root` above for the rationale.
        hasher.update(b"compiler\0");
        hasher.update(self.compiler_hash.as_bytes());
        hasher.update(b"\0");

        if self
            .codegen_flags
            .iter()
            .any(|flag| is_rustc_native_cpu_flag(flag))
        {
            let salt = match native_cpu_salt {
                Some(salt) => salt,
                None => host_cpu_identity_salt(),
            };
            hasher.update(b"native-cpu-host\0");
            hasher.update(salt.as_bytes());
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
            if VOLATILE_CARGO_ENV_VARS.contains(&key.as_str()) || key == DYLINT_CACHE_INPUT_HASH_ENV
            {
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

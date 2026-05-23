//! Compilation context and cache key computation.
//!
//! The context key identifies a unique (source + flags) combination
//! and maps to an include list. The artifact key incorporates content
//! hashes of all files for artifact store lookup.

use std::path::Path;

use zccache_monocrate::core::path::normalize_for_key;
use zccache_monocrate::core::NormalizedPath;
use zccache_monocrate::hash::ContentHash;

use crate::args::ParsedArgs;
use crate::rustc_args::RustcParsedArgs;
use crate::search_paths::IncludeSearchPaths;

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
    /// Sorted unknown flags â€” not recognized by the parser but still
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
    /// sorted flags, unknown flags, force includes.
    #[must_use]
    pub fn context_key(&self) -> ContextKey {
        compute_context_key(self, None)
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

fn normalize_key_path(path: &Path, key_root: Option<&Path>) -> String {
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
#[must_use]
pub fn compute_context_key(ctx: &CompileContext, key_root: Option<&Path>) -> ContextKey {
    let mut hasher = blake3::Hasher::new();

    hasher.update(b"zccache-context-key-v1\0");

    hasher.update(normalize_key_path(&ctx.source_file, key_root).as_bytes());
    hasher.update(b"\0");

    hasher.update(b"iquote\0");
    for dir in &ctx.include_search.iquote {
        hasher.update(normalize_key_path(dir, key_root).as_bytes());
        hasher.update(b"\0");
    }

    hasher.update(b"user\0");
    for dir in &ctx.include_search.user {
        hasher.update(normalize_key_path(dir, key_root).as_bytes());
        hasher.update(b"\0");
    }

    hasher.update(b"system\0");
    for dir in &ctx.include_search.system {
        hasher.update(normalize_key_path(dir, key_root).as_bytes());
        hasher.update(b"\0");
    }

    hasher.update(b"after\0");
    for dir in &ctx.include_search.after {
        hasher.update(normalize_key_path(dir, key_root).as_bytes());
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
        hasher.update(normalize_key_path(fi, key_root).as_bytes());
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
    // Sort by path for determinism.
    file_hashes.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"zccache-artifact-key-v1\0");
    hasher.update(context_key.0.as_bytes());
    hasher.update(b"\0");

    for (path, hash) in file_hashes.iter() {
        let path = normalize_key_path(path.as_ref(), key_root);
        hasher.update(path.as_bytes());
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
const VOLATILE_CARGO_ENV_VARS: &[&str] = &["CARGO_MANIFEST_DIR", "CARGO_MANIFEST_PATH"];

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

        // Filter CARGO_* env vars â€” these affect compilation output via env!() macro.
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
    if key_root.is_some() {
        file_hashes.sort_by(|a, b| {
            normalize_key_path(a.0.as_ref(), key_root)
                .cmp(&normalize_key_path(b.0.as_ref(), key_root))
        });
    } else {
        file_hashes.sort_by(|a, b| a.0.cmp(&b.0));
    }
    extern_hashes.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"zccache-rustc-artifact-key-v1\0");
    hasher.update(context_key.0.as_bytes());
    hasher.update(b"\0");

    // Source + dependency file hashes.
    for (path, hash) in file_hashes.iter() {
        let path = normalize_key_path(path.as_ref(), key_root);
        hasher.update(path.as_bytes());
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

#[cfg(test)]
mod tests {
    use zccache_monocrate::core::NormalizedPath;

    use super::*;
    use crate::args::UserDepFlags;
    use crate::rustc_args::ExternCrate;

    fn make_context(source: &str, user_dirs: &[&str], defines: &[&str]) -> CompileContext {
        CompileContext {
            source_file: NormalizedPath::from(source),
            include_search: IncludeSearchPaths {
                user: user_dirs
                    .iter()
                    .map(|dir| NormalizedPath::from(*dir))
                    .collect(),
                ..Default::default()
            },
            defines: {
                let mut d: Vec<String> = defines.iter().map(|s| s.to_string()).collect();
                d.sort();
                d
            },
            flags: Vec::new(),
            force_includes: Vec::new(),
            unknown_flags: Vec::new(),
        }
    }

    #[test]
    fn context_key_deterministic() {
        let ctx = make_context("/src/foo.c", &["/inc"], &["DEBUG"]);
        let k1 = ctx.context_key();
        let k2 = ctx.context_key();
        assert_eq!(k1, k2);
    }

    #[test]
    fn different_source_different_key() {
        let k1 = make_context("/src/a.c", &["/inc"], &[]).context_key();
        let k2 = make_context("/src/b.c", &["/inc"], &[]).context_key();
        assert_ne!(k1, k2);
    }

    #[test]
    fn different_defines_different_key() {
        let k1 = make_context("/src/a.c", &["/inc"], &["DEBUG"]).context_key();
        let k2 = make_context("/src/a.c", &["/inc"], &["RELEASE"]).context_key();
        assert_ne!(k1, k2);
    }

    #[test]
    fn define_order_irrelevant() {
        let k1 = make_context("/src/a.c", &[], &["AAA", "BBB"]).context_key();
        let k2 = make_context("/src/a.c", &[], &["BBB", "AAA"]).context_key();
        assert_eq!(k1, k2, "define order should not affect context key");
    }

    #[test]
    fn include_dir_order_matters() {
        let k1 = make_context("/src/a.c", &["/first", "/second"], &[]).context_key();
        let k2 = make_context("/src/a.c", &["/second", "/first"], &[]).context_key();
        assert_ne!(k1, k2, "include dir order should affect context key");
    }

    #[cfg(windows)]
    #[test]
    fn windows_context_key_normalizes_equivalent_path_spellings() {
        let ctx1 = CompileContext {
            source_file: NormalizedPath::from(r"C:\work\src\main.cpp"),
            include_search: IncludeSearchPaths {
                user: vec![NormalizedPath::from(r"C:\work\include")],
                ..Default::default()
            },
            defines: Vec::new(),
            flags: Vec::new(),
            force_includes: vec![NormalizedPath::from(r"C:\work\pch\base.h")],
            unknown_flags: Vec::new(),
        };
        let ctx2 = CompileContext {
            source_file: NormalizedPath::from("c:/work/src/main.cpp"),
            include_search: IncludeSearchPaths {
                user: vec![NormalizedPath::from("c:/work/include")],
                ..Default::default()
            },
            defines: Vec::new(),
            flags: Vec::new(),
            force_includes: vec![NormalizedPath::from("c:/work/pch/base.h")],
            unknown_flags: Vec::new(),
        };

        assert_eq!(ctx1.context_key(), ctx2.context_key());
    }

    #[cfg(windows)]
    #[test]
    fn windows_artifact_key_normalizes_equivalent_path_spellings() {
        let ctx = CompileContext {
            source_file: NormalizedPath::from(r"C:\work\src\main.cpp"),
            include_search: IncludeSearchPaths::default(),
            defines: Vec::new(),
            flags: Vec::new(),
            force_includes: Vec::new(),
            unknown_flags: Vec::new(),
        };
        let key = ctx.context_key();

        let mut file_hashes_a = vec![
            (
                NormalizedPath::from(r"C:\work\include\foo.h"),
                zccache_monocrate::hash::hash_bytes(b"header"),
            ),
            (
                NormalizedPath::from(r"C:\work\src\main.cpp"),
                zccache_monocrate::hash::hash_bytes(b"source"),
            ),
        ];
        let mut file_hashes_b = vec![
            (
                NormalizedPath::from("c:/work/include/foo.h"),
                zccache_monocrate::hash::hash_bytes(b"header"),
            ),
            (
                NormalizedPath::from("c:/work/src/main.cpp"),
                zccache_monocrate::hash::hash_bytes(b"source"),
            ),
        ];

        assert_eq!(
            compute_artifact_key(&key, &mut file_hashes_a, None),
            compute_artifact_key(&key, &mut file_hashes_b, None)
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_rustc_context_key_normalizes_equivalent_source_path_spellings() {
        let ctx1 = RustcCompileContext {
            source_file: NormalizedPath::from(r"C:\work\src\lib.rs"),
            crate_name: Some("demo".to_string()),
            crate_types: vec!["rlib".to_string()],
            edition: Some("2021".to_string()),
            emit_types: vec!["link".to_string()],
            cfgs: Vec::new(),
            check_cfgs: Vec::new(),
            codegen_flags: Vec::new(),
            cargo_metadata: None,
            extra_filename: None,
            target: None,
            cap_lints: None,
            extern_crates: Vec::new(),
            lint_flags: Vec::new(),
            unknown_flags: Vec::new(),
            remap_path_prefixes: Vec::new(),
            env_vars: Vec::new(),
            compiler_hash: None,
        };
        let mut ctx2 = ctx1.clone();
        ctx2.source_file = NormalizedPath::from("c:/work/src/lib.rs");

        assert_eq!(ctx1.context_key(), ctx2.context_key());
    }

    #[test]
    fn artifact_key_changes_with_content() {
        let ctx = make_context("/src/a.c", &[], &[]);
        let ck = ctx.context_key();

        let hash_a = zccache_monocrate::hash::hash_bytes(b"content A");
        let hash_b = zccache_monocrate::hash::hash_bytes(b"content B");

        let ak1 =
            compute_artifact_key(&ck, &mut [(NormalizedPath::from("/src/a.c"), hash_a)], None);
        let ak2 =
            compute_artifact_key(&ck, &mut [(NormalizedPath::from("/src/a.c"), hash_b)], None);
        assert_ne!(ak1, ak2);
    }

    #[test]
    fn artifact_key_stable_same_content() {
        let ctx = make_context("/src/a.c", &[], &[]);
        let ck = ctx.context_key();

        let hash = zccache_monocrate::hash::hash_bytes(b"content");

        let ak1 = compute_artifact_key(&ck, &mut [(NormalizedPath::from("/src/a.c"), hash)], None);
        let ak2 = compute_artifact_key(&ck, &mut [(NormalizedPath::from("/src/a.c"), hash)], None);
        assert_eq!(ak1, ak2);
    }

    #[test]
    fn artifact_key_file_order_irrelevant() {
        let ctx = make_context("/src/a.c", &[], &[]);
        let ck = ctx.context_key();

        let h1 = zccache_monocrate::hash::hash_bytes(b"content 1");
        let h2 = zccache_monocrate::hash::hash_bytes(b"content 2");

        let ak1 = compute_artifact_key(
            &ck,
            &mut [
                (NormalizedPath::from("/a.h"), h1),
                (NormalizedPath::from("/b.h"), h2),
            ],
            None,
        );
        let ak2 = compute_artifact_key(
            &ck,
            &mut [
                (NormalizedPath::from("/b.h"), h2),
                (NormalizedPath::from("/a.h"), h1),
            ],
            None,
        );
        assert_eq!(ak1, ak2, "file order should not affect artifact key");
    }

    #[test]
    fn context_key_ignores_workspace_root_when_key_root_is_stable() {
        let ctx_a = make_context(
            "/workspace-a/src/main.cpp",
            &["/workspace-a/include"],
            &["DEBUG"],
        );
        let ctx_b = make_context(
            "/workspace-b/src/main.cpp",
            &["/workspace-b/include"],
            &["DEBUG"],
        );

        let key_a = compute_context_key(&ctx_a, Some(Path::new("/workspace-a")));
        let key_b = compute_context_key(&ctx_b, Some(Path::new("/workspace-b")));

        assert_eq!(key_a, key_b);
    }

    #[test]
    fn cxx_context_key_with_root_normalizes_file_prefix_map_roots() {
        let mut ctx_a = make_context("/workspace-a/src/main.cpp", &[], &[]);
        ctx_a.flags = vec!["-ffile-prefix-map=/workspace-a=.".to_string()];
        let mut ctx_b = make_context("/workspace-b/src/main.cpp", &[], &[]);
        ctx_b.flags = vec!["-ffile-prefix-map=/workspace-b=.".to_string()];

        assert_eq!(
            compute_context_key(&ctx_a, Some(Path::new("/workspace-a"))),
            compute_context_key(&ctx_b, Some(Path::new("/workspace-b"))),
            "equivalent file-prefix-map old prefixes under the key root should match"
        );
    }

    #[test]
    fn cxx_context_key_with_root_preserves_file_prefix_map_new_prefixes() {
        let mut ctx_a = make_context("/workspace-a/src/main.cpp", &[], &[]);
        ctx_a.flags = vec!["-ffile-prefix-map=/workspace-a=.".to_string()];
        let mut ctx_b = make_context("/workspace-b/src/main.cpp", &[], &[]);
        ctx_b.flags = vec!["-ffile-prefix-map=/workspace-b=/src".to_string()];

        assert_ne!(
            compute_context_key(&ctx_a, Some(Path::new("/workspace-a"))),
            compute_context_key(&ctx_b, Some(Path::new("/workspace-b"))),
            "different file-prefix-map new prefixes should remain key-significant"
        );
    }

    #[test]
    fn cxx_context_key_with_root_keeps_external_file_prefix_map_old_prefixes_distinct() {
        let mut ctx_a = make_context("/workspace-a/src/main.cpp", &[], &[]);
        ctx_a.flags = vec!["-ffile-prefix-map=/external-a=.".to_string()];
        let mut ctx_b = make_context("/workspace-b/src/main.cpp", &[], &[]);
        ctx_b.flags = vec!["-ffile-prefix-map=/external-b=.".to_string()];

        assert_ne!(
            compute_context_key(&ctx_a, Some(Path::new("/workspace-a"))),
            compute_context_key(&ctx_b, Some(Path::new("/workspace-b"))),
            "file-prefix-map old prefixes outside the key root should keep absolute identity"
        );
    }

    #[test]
    fn cxx_context_key_with_root_normalizes_prefix_maps_in_unknown_flags() {
        let mut ctx_a = make_context("/workspace-a/src/main.cpp", &[], &[]);
        ctx_a.unknown_flags = vec![
            "-fcoverage-prefix-map=/workspace-a=/coverage".to_string(),
            "-fdebug-prefix-map=/workspace-a=/debug".to_string(),
            "-fmacro-prefix-map=/workspace-a=/macro".to_string(),
            "-fprofile-prefix-map=/workspace-a=/profile".to_string(),
        ];
        let mut ctx_b = make_context("/workspace-b/src/main.cpp", &[], &[]);
        ctx_b.unknown_flags = vec![
            "-fcoverage-prefix-map=/workspace-b=/coverage".to_string(),
            "-fdebug-prefix-map=/workspace-b=/debug".to_string(),
            "-fmacro-prefix-map=/workspace-b=/macro".to_string(),
            "-fprofile-prefix-map=/workspace-b=/profile".to_string(),
        ];

        assert_eq!(
            compute_context_key(&ctx_a, Some(Path::new("/workspace-a"))),
            compute_context_key(&ctx_b, Some(Path::new("/workspace-b"))),
            "C/C++ prefix-map flags should normalize under unknown_flags"
        );
    }

    #[test]
    fn artifact_key_ignores_workspace_root_when_key_root_is_stable() {
        let ctx = make_context("/workspace-a/src/main.cpp", &["/workspace-a/include"], &[]);
        let key = compute_context_key(&ctx, Some(Path::new("/workspace-a")));
        let mut hashes_a = vec![
            (
                NormalizedPath::from("/workspace-a/include/foo.h"),
                zccache_monocrate::hash::hash_bytes(b"header"),
            ),
            (
                NormalizedPath::from("/workspace-a/src/main.cpp"),
                zccache_monocrate::hash::hash_bytes(b"source"),
            ),
        ];
        let mut hashes_b = vec![
            (
                NormalizedPath::from("/workspace-b/include/foo.h"),
                zccache_monocrate::hash::hash_bytes(b"header"),
            ),
            (
                NormalizedPath::from("/workspace-b/src/main.cpp"),
                zccache_monocrate::hash::hash_bytes(b"source"),
            ),
        ];

        assert_eq!(
            compute_artifact_key(&key, &mut hashes_a, Some(Path::new("/workspace-a"))),
            compute_artifact_key(&key, &mut hashes_b, Some(Path::new("/workspace-b")))
        );
    }

    #[test]
    fn context_key_display() {
        let ctx = make_context("/src/a.c", &[], &[]);
        let key = ctx.context_key();
        let display = format!("{key}");
        assert!(display.starts_with("ctx:"));
        assert_eq!(display.len(), 4 + 64); // "ctx:" + 64 hex chars
    }

    #[test]
    fn from_parsed_args_sorts() {
        let args = ParsedArgs {
            source_file: NormalizedPath::from("/src/a.c"),
            output_file: None,
            include_search: IncludeSearchPaths::default(),
            defines: vec!["ZZZ".into(), "AAA".into()],
            undefines: Vec::new(),
            flags: vec!["-Wall".into(), "-O2".into()],
            force_includes: Vec::new(),
            compiler: None,
            dep_flags: UserDepFlags::default(),
            unknown_flags: vec!["--zzz".into(), "--aaa".into()],
        };
        let ctx = CompileContext::from_parsed_args(args);
        assert_eq!(ctx.defines, vec!["AAA", "ZZZ"]);
        assert_eq!(ctx.flags, vec!["-O2", "-Wall"]);
        assert_eq!(ctx.unknown_flags, vec!["--aaa", "--zzz"]);
    }

    #[test]
    fn different_flags_different_key() {
        let mut ctx1 = make_context("/src/a.c", &[], &[]);
        ctx1.flags = vec!["-std=c++17".into()];
        let mut ctx2 = make_context("/src/a.c", &[], &[]);
        ctx2.flags = vec!["-std=c++20".into()];
        assert_ne!(ctx1.context_key(), ctx2.context_key());
    }

    #[test]
    fn force_include_affects_key() {
        let ctx1 = make_context("/src/a.c", &[], &[]);
        let mut ctx2 = make_context("/src/a.c", &[], &[]);
        ctx2.force_includes = vec![NormalizedPath::from("/pch.h")];
        assert_ne!(ctx1.context_key(), ctx2.context_key());
    }

    #[test]
    fn unknown_flags_affect_key() {
        let ctx1 = make_context("/src/a.c", &[], &[]);
        let mut ctx2 = make_context("/src/a.c", &[], &[]);
        ctx2.unknown_flags = vec!["--deploy-dependencies".into()];
        assert_ne!(
            ctx1.context_key(),
            ctx2.context_key(),
            "unknown flags should affect context key"
        );
    }

    #[test]
    fn unknown_flags_order_irrelevant() {
        let mut ctx1 = make_context("/src/a.c", &[], &[]);
        ctx1.unknown_flags = vec!["--aaa".into(), "--bbb".into()];
        let mut ctx2 = make_context("/src/a.c", &[], &[]);
        ctx2.unknown_flags = vec!["--bbb".into(), "--aaa".into()];
        // Both are sorted in make_context... but actually make_context doesn't sort unknown_flags.
        // from_parsed_args sorts them. In the test helper we set them directly,
        // so we need to sort manually for this test to be meaningful.
        ctx1.unknown_flags.sort();
        ctx2.unknown_flags.sort();
        assert_eq!(
            ctx1.context_key(),
            ctx2.context_key(),
            "unknown flag order should not affect context key"
        );
    }

    // â”€â”€â”€ RustcCompileContext tests â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    fn make_rustc_context(source: &str, edition: &str) -> RustcCompileContext {
        RustcCompileContext {
            source_file: NormalizedPath::from(source),
            crate_name: Some("mylib".to_string()),
            crate_types: vec!["lib".to_string()],
            edition: Some(edition.to_string()),
            emit_types: vec!["link".to_string()],
            cfgs: Vec::new(),
            check_cfgs: Vec::new(),
            codegen_flags: Vec::new(),
            cargo_metadata: None,
            extra_filename: None,
            target: None,
            cap_lints: None,
            extern_crates: Vec::new(),
            lint_flags: Vec::new(),
            unknown_flags: Vec::new(),
            remap_path_prefixes: Vec::new(),
            env_vars: Vec::new(),
            compiler_hash: None,
        }
    }

    #[test]
    fn rustc_context_key_deterministic() {
        let ctx = make_rustc_context("/src/lib.rs", "2021");
        let k1 = ctx.context_key();
        let k2 = ctx.context_key();
        assert_eq!(k1, k2);
    }

    #[test]
    fn rustc_context_key_delegates_to_rootless_helper() {
        let ctx = make_rustc_context("/src/lib.rs", "2021");
        assert_eq!(ctx.context_key(), ctx.context_key_with_root(None));
    }

    #[test]
    fn rustc_context_key_with_root_matches_equivalent_roots() {
        let ctx_a = make_rustc_context("/workspace-a/crates/demo/src/lib.rs", "2021");
        let ctx_b = make_rustc_context("/workspace-b/crates/demo/src/lib.rs", "2021");

        assert_ne!(
            ctx_a.context_key(),
            ctx_b.context_key(),
            "rootless rustc context keys should keep the existing absolute-path behavior"
        );
        assert_eq!(
            ctx_a.context_key_with_root(Some(Path::new("/workspace-a"))),
            ctx_b.context_key_with_root(Some(Path::new("/workspace-b"))),
            "source paths under equivalent roots should hash relative to those roots"
        );
    }

    #[test]
    fn rustc_context_key_with_root_keeps_external_sources_distinct() {
        let ctx_a = make_rustc_context("/external-a/generated/lib.rs", "2021");
        let ctx_b = make_rustc_context("/external-b/generated/lib.rs", "2021");

        assert_ne!(
            ctx_a.context_key_with_root(Some(Path::new("/workspace-a"))),
            ctx_b.context_key_with_root(Some(Path::new("/workspace-b"))),
            "sources outside the supplied roots must retain absolute path identity"
        );
    }

    #[test]
    fn rustc_context_key_with_root_normalizes_remap_left_side_under_root() {
        let mut ctx_a = make_rustc_context("/workspace-a/crates/demo/src/lib.rs", "2021");
        ctx_a.remap_path_prefixes = vec!["/workspace-a=/src".to_string()];
        let mut ctx_b = make_rustc_context("/workspace-b/crates/demo/src/lib.rs", "2021");
        ctx_b.remap_path_prefixes = vec!["/workspace-b=/src".to_string()];

        assert_eq!(
            ctx_a.context_key_with_root(Some(Path::new("/workspace-a"))),
            ctx_b.context_key_with_root(Some(Path::new("/workspace-b"))),
            "remap left sides under the root should hash relative to the root"
        );
    }

    #[test]
    fn rustc_context_key_with_root_normalizes_root_remap_left_side() {
        let mut ctx_a = make_rustc_context("/workspace-a/crates/demo/src/lib.rs", "2021");
        ctx_a.remap_path_prefixes = vec!["/workspace-a=.".to_string()];
        let mut ctx_b = make_rustc_context("/workspace-b/crates/demo/src/lib.rs", "2021");
        ctx_b.remap_path_prefixes = vec!["/workspace-b=.".to_string()];

        assert_eq!(
            ctx_a.context_key_with_root(Some(Path::new("/workspace-a"))),
            ctx_b.context_key_with_root(Some(Path::new("/workspace-b"))),
            "root-covering remaps should hash equivalently across roots"
        );
    }

    #[test]
    fn rustc_context_key_with_root_keeps_external_remap_left_sides_distinct() {
        let mut ctx_a = make_rustc_context("/workspace-a/crates/demo/src/lib.rs", "2021");
        ctx_a.remap_path_prefixes = vec!["/external-a=/src".to_string()];
        let mut ctx_b = make_rustc_context("/workspace-b/crates/demo/src/lib.rs", "2021");
        ctx_b.remap_path_prefixes = vec!["/external-b=/src".to_string()];

        assert_ne!(
            ctx_a.context_key_with_root(Some(Path::new("/workspace-a"))),
            ctx_b.context_key_with_root(Some(Path::new("/workspace-b"))),
            "remap left sides outside the root should keep absolute path identity"
        );
    }

    #[test]
    fn rustc_context_key_with_root_does_not_normalize_remap_right_side() {
        let mut ctx_a = make_rustc_context("/workspace-a/crates/demo/src/lib.rs", "2021");
        ctx_a.remap_path_prefixes = vec!["/workspace-a=/workspace-a".to_string()];
        let mut ctx_b = make_rustc_context("/workspace-b/crates/demo/src/lib.rs", "2021");
        ctx_b.remap_path_prefixes = vec!["/workspace-b=/workspace-b".to_string()];

        assert_ne!(
            ctx_a.context_key_with_root(Some(Path::new("/workspace-a"))),
            ctx_b.context_key_with_root(Some(Path::new("/workspace-b"))),
            "only the remap left side is root-normalized"
        );
    }

    #[test]
    fn rustc_context_key_with_root_preserves_remap_right_side() {
        let mut ctx_a = make_rustc_context("/workspace-a/crates/demo/src/lib.rs", "2021");
        ctx_a.remap_path_prefixes = vec!["/workspace-a=.".to_string()];
        let mut ctx_b = make_rustc_context("/workspace-b/crates/demo/src/lib.rs", "2021");
        ctx_b.remap_path_prefixes = vec!["/workspace-b=/src".to_string()];

        assert_ne!(
            ctx_a.context_key_with_root(Some(Path::new("/workspace-a"))),
            ctx_b.context_key_with_root(Some(Path::new("/workspace-b"))),
            "different remap new prefixes must remain cache-significant"
        );
    }

    #[test]
    fn rustc_context_key_with_root_keeps_malformed_remaps_distinct() {
        let mut ctx_a = make_rustc_context("/workspace-a/crates/demo/src/lib.rs", "2021");
        ctx_a.remap_path_prefixes = vec!["/workspace-a".to_string()];
        let mut ctx_b = make_rustc_context("/workspace-b/crates/demo/src/lib.rs", "2021");
        ctx_b.remap_path_prefixes = vec!["/workspace-b".to_string()];

        assert_ne!(
            ctx_a.context_key_with_root(Some(Path::new("/workspace-a"))),
            ctx_b.context_key_with_root(Some(Path::new("/workspace-b"))),
            "malformed remap values should not be root-normalized"
        );
    }

    #[test]
    fn rustc_different_edition_different_key() {
        let k1 = make_rustc_context("/src/lib.rs", "2021").context_key();
        let k2 = make_rustc_context("/src/lib.rs", "2024").context_key();
        assert_ne!(k1, k2);
    }

    #[test]
    fn rustc_different_cfg_different_key() {
        let mut ctx1 = make_rustc_context("/src/lib.rs", "2021");
        ctx1.cfgs = vec!["feature=\"std\"".to_string()];
        let mut ctx2 = make_rustc_context("/src/lib.rs", "2021");
        ctx2.cfgs = vec!["feature=\"alloc\"".to_string()];
        assert_ne!(ctx1.context_key(), ctx2.context_key());
    }

    #[test]
    fn rustc_different_codegen_different_key() {
        let mut ctx1 = make_rustc_context("/src/lib.rs", "2021");
        ctx1.codegen_flags = vec!["opt-level=2".to_string()];
        let mut ctx2 = make_rustc_context("/src/lib.rs", "2021");
        ctx2.codegen_flags = vec!["opt-level=3".to_string()];
        assert_ne!(ctx1.context_key(), ctx2.context_key());
    }

    #[test]
    fn rustc_cargo_metadata_affects_key() {
        let mut ctx1 = make_rustc_context("/src/lib.rs", "2021");
        ctx1.cargo_metadata = Some("worktree-a".to_string());
        let mut ctx2 = make_rustc_context("/src/lib.rs", "2021");
        ctx2.cargo_metadata = Some("worktree-b".to_string());
        assert_ne!(
            ctx1.context_key(),
            ctx2.context_key(),
            "-C metadata participates in crate disambiguation and must affect the key"
        );
    }

    #[test]
    fn rustc_extra_filename_affects_key() {
        let mut ctx1 = make_rustc_context("/src/lib.rs", "2021");
        ctx1.extra_filename = Some("-aaa111".to_string());
        let mut ctx2 = make_rustc_context("/src/lib.rs", "2021");
        ctx2.extra_filename = Some("-bbb222".to_string());
        assert_ne!(
            ctx1.context_key(),
            ctx2.context_key(),
            "-C extra-filename controls emitted artifact names and must affect the key"
        );
    }

    #[test]
    fn rustc_context_key_differs_from_cc() {
        // The domain separation tags differ, so even identical-looking contexts
        // produce different keys.
        let cc_ctx = make_context("/src/lib.rs", &[], &[]);
        let rustc_ctx = make_rustc_context("/src/lib.rs", "2021");
        assert_ne!(
            cc_ctx.context_key(),
            rustc_ctx.context_key(),
            "C and Rust context keys must differ (domain separation)"
        );
    }

    #[test]
    fn rustc_compiler_hash_affects_key() {
        let ctx1 = make_rustc_context("/src/lib.rs", "2021");
        let mut ctx2 = make_rustc_context("/src/lib.rs", "2021");
        ctx2.compiler_hash = Some(zccache_monocrate::hash::hash_bytes(b"rustc-1.94.1"));
        assert_ne!(
            ctx1.context_key(),
            ctx2.context_key(),
            "different compiler hash must produce different context key"
        );
    }

    #[test]
    fn rustc_different_compiler_versions_different_key() {
        let mut ctx1 = make_rustc_context("/src/lib.rs", "2021");
        ctx1.compiler_hash = Some(zccache_monocrate::hash::hash_bytes(b"rustc-1.94.1"));
        let mut ctx2 = make_rustc_context("/src/lib.rs", "2021");
        ctx2.compiler_hash = Some(zccache_monocrate::hash::hash_bytes(b"rustc-1.94.2"));
        assert_ne!(ctx1.context_key(), ctx2.context_key());
    }

    #[test]
    fn rustc_extern_crates_affect_key() {
        let ctx1 = make_rustc_context("/src/lib.rs", "2021");
        let mut ctx2 = make_rustc_context("/src/lib.rs", "2021");
        ctx2.extern_crates = vec![("serde".into(), "/deps/libserde.rlib".into())];
        assert_ne!(ctx1.context_key(), ctx2.context_key());
    }

    #[test]
    fn rustc_different_extern_paths_different_key() {
        let mut ctx1 = make_rustc_context("/src/lib.rs", "2021");
        ctx1.extern_crates = vec![("a".into(), "/deps/liba_v1.rlib".into())];
        let mut ctx2 = make_rustc_context("/src/lib.rs", "2021");
        ctx2.extern_crates = vec![("a".into(), "/deps/liba_v2.rlib".into())];
        assert_ne!(
            ctx1.context_key(),
            ctx2.context_key(),
            "different extern paths must produce different context keys"
        );
    }

    #[test]
    fn rustc_from_parsed_args() {
        let args = RustcParsedArgs {
            source_file: NormalizedPath::from("/src/lib.rs"),
            crate_name: Some("mylib".to_string()),
            crate_types: vec!["rlib".to_string(), "lib".to_string()],
            edition: Some("2021".to_string()),
            emit_types: vec!["link".to_string(), "dep-info".to_string()],
            cfgs: vec!["unix".to_string(), "feature=\"std\"".to_string()],
            check_cfgs: Vec::new(),
            codegen_flags: vec!["opt-level=2".to_string()],
            target: None,
            cap_lints: Some("allow".to_string()),
            externs: vec![
                ExternCrate {
                    name: "serde".to_string(),
                    path: NormalizedPath::from("/deps/libserde.rlib"),
                },
                ExternCrate {
                    name: "log".to_string(),
                    path: NormalizedPath::from("/deps/liblog.rlib"),
                },
            ],
            lint_flags: Vec::new(),
            unknown_flags: Vec::new(),
            out_dir: None,
            extra_filename: Some("-abc123".to_string()),
            cargo_metadata: Some("abc123".to_string()),
            incremental_dir: None,
            error_format: None,
            json_format: None,
            color: None,
            diagnostic_width: None,
            search_paths: Vec::new(),
            remap_path_prefixes: Vec::new(),
            sysroot: None,
            output_file: None,
        };
        let ctx = RustcCompileContext::from_parsed_args(&args, &[], None);
        // Crate types sorted
        assert_eq!(ctx.crate_types, vec!["lib", "rlib"]);
        // Emit types sorted
        assert_eq!(ctx.emit_types, vec!["dep-info", "link"]);
        // Extern crates extracted and sorted by name
        assert_eq!(ctx.extern_crates.len(), 2);
        assert_eq!(ctx.extern_crates[0].0, "log");
        assert_eq!(ctx.extern_crates[1].0, "serde");
        assert_eq!(ctx.cargo_metadata.as_deref(), Some("abc123"));
        assert_eq!(ctx.extra_filename.as_deref(), Some("-abc123"));
    }

    #[test]
    fn rustc_artifact_key_changes_with_extern_content() {
        let ctx = make_rustc_context("/src/lib.rs", "2021");
        let ck = ctx.context_key();

        let src_hash = zccache_monocrate::hash::hash_bytes(b"source");
        let ext_hash_a = zccache_monocrate::hash::hash_bytes(b"extern A");
        let ext_hash_b = zccache_monocrate::hash::hash_bytes(b"extern B");

        let ak1 = compute_rustc_artifact_key(
            &ck,
            &mut [(NormalizedPath::from("/src/lib.rs"), src_hash)],
            &mut [("serde".to_string(), ext_hash_a)],
        );
        let ak2 = compute_rustc_artifact_key(
            &ck,
            &mut [(NormalizedPath::from("/src/lib.rs"), src_hash)],
            &mut [("serde".to_string(), ext_hash_b)],
        );
        assert_ne!(
            ak1, ak2,
            "different extern content should produce different artifact key"
        );
    }

    // ─── Cache-key path-independence tests (issue #139, fix #1) ────────────────
    //
    // These tests pin the contract that cache keys are independent of the absolute
    // path at which a workspace happens to live on disk. The same project checked
    // out at `/tmp/proj-a` and `/tmp/proj-b` must produce the same rustc cache key,
    // otherwise every `cargo {check,clippy,test}` after moving / re-cloning the
    // repo cold-misses through the entire dep graph.

    fn make_rustc_context_with_env(env: Vec<(String, String)>) -> RustcCompileContext {
        let mut ctx = make_rustc_context("/src/lib.rs", "2021");
        ctx.env_vars = env;
        ctx.env_vars.sort();
        ctx
    }

    /// T1 — Two contexts that differ only in `CARGO_MANIFEST_DIR` must have
    /// the same cache key. This is the headline regression: the same crate
    /// checked out at two paths should not invalidate the cache.
    #[test]
    fn rustc_context_key_ignores_cargo_manifest_dir() {
        let ctx_a = make_rustc_context_with_env(vec![
            ("CARGO_MANIFEST_DIR".into(), "/tmp/proj-a/crates/foo".into()),
            ("CARGO_PKG_NAME".into(), "foo".into()),
            ("CARGO_PKG_VERSION".into(), "1.2.3".into()),
        ]);
        let ctx_b = make_rustc_context_with_env(vec![
            ("CARGO_MANIFEST_DIR".into(), "/tmp/proj-b/crates/foo".into()),
            ("CARGO_PKG_NAME".into(), "foo".into()),
            ("CARGO_PKG_VERSION".into(), "1.2.3".into()),
        ]);
        assert_eq!(
            ctx_a.context_key(),
            ctx_b.context_key(),
            "CARGO_MANIFEST_DIR is volatile (absolute path) and must NOT \
             contribute to the cache key; otherwise a project clone or rename \
             invalidates every dependent compile"
        );
    }

    /// T2 — Same idea for `CARGO_MANIFEST_PATH`. Cargo started exporting this
    /// in newer versions; it's similarly an absolute path to `Cargo.toml`.
    #[test]
    fn rustc_context_key_ignores_cargo_manifest_path() {
        let ctx_a = make_rustc_context_with_env(vec![
            (
                "CARGO_MANIFEST_PATH".into(),
                "/tmp/proj-a/crates/foo/Cargo.toml".into(),
            ),
            ("CARGO_PKG_NAME".into(), "foo".into()),
            ("CARGO_PKG_VERSION".into(), "1.2.3".into()),
        ]);
        let ctx_b = make_rustc_context_with_env(vec![
            (
                "CARGO_MANIFEST_PATH".into(),
                "/tmp/proj-b/crates/foo/Cargo.toml".into(),
            ),
            ("CARGO_PKG_NAME".into(), "foo".into()),
            ("CARGO_PKG_VERSION".into(), "1.2.3".into()),
        ]);
        assert_eq!(
            ctx_a.context_key(),
            ctx_b.context_key(),
            "CARGO_MANIFEST_PATH is volatile (absolute path) and must NOT \
             contribute to the cache key"
        );
    }

    /// T3 — Negative control: `CARGO_PKG_VERSION` MUST still affect the key
    /// because `env!("CARGO_PKG_VERSION")` is embedded in compiled output.
    /// This guards against an over-eager filter that strips too much.
    #[test]
    fn rustc_context_key_sensitive_to_cargo_pkg_version() {
        let ctx_a = make_rustc_context_with_env(vec![("CARGO_PKG_VERSION".into(), "1.2.3".into())]);
        let ctx_b = make_rustc_context_with_env(vec![("CARGO_PKG_VERSION".into(), "1.2.4".into())]);
        assert_ne!(
            ctx_a.context_key(),
            ctx_b.context_key(),
            "CARGO_PKG_VERSION feeds env!() macros and MUST be in the cache key"
        );
    }

    /// T4 — Extern rmeta paths that share a filename (and therefore the same
    /// `metadata=` hash from cargo) but differ in their absolute directory
    /// prefix must produce equal cache keys. This is the cascade-killer: when
    /// a dep crate is rebuilt at the same content but in a different target
    /// dir, all downstream crates should still hit.
    #[test]
    fn rustc_context_key_ignores_extern_directory_prefix() {
        let mut ctx_a = make_rustc_context("/src/lib.rs", "2021");
        ctx_a.extern_crates = vec![(
            "serde".into(),
            "/tmp/proj-a/target/debug/deps/libserde-abc123.rmeta".into(),
        )];
        let mut ctx_b = make_rustc_context("/src/lib.rs", "2021");
        ctx_b.extern_crates = vec![(
            "serde".into(),
            "/tmp/proj-b/target/debug/deps/libserde-abc123.rmeta".into(),
        )];
        assert_eq!(
            ctx_a.context_key(),
            ctx_b.context_key(),
            "extern rmeta paths with the same filename (= same cargo metadata \
             hash) but different absolute prefixes must produce equal cache \
             keys; otherwise relocating the workspace cascades through every \
             downstream crate"
        );
    }

    #[test]
    fn rustc_artifact_key_stable() {
        let ctx = make_rustc_context("/src/lib.rs", "2021");
        let ck = ctx.context_key();

        let src_hash = zccache_monocrate::hash::hash_bytes(b"source");
        let ext_hash = zccache_monocrate::hash::hash_bytes(b"extern");

        let ak1 = compute_rustc_artifact_key(
            &ck,
            &mut [(NormalizedPath::from("/src/lib.rs"), src_hash)],
            &mut [("serde".to_string(), ext_hash)],
        );
        let ak2 = compute_rustc_artifact_key(
            &ck,
            &mut [(NormalizedPath::from("/src/lib.rs"), src_hash)],
            &mut [("serde".to_string(), ext_hash)],
        );
        assert_eq!(ak1, ak2);
    }

    #[test]
    fn rustc_artifact_key_with_root_matches_equivalent_source_and_dependency_paths() {
        let ctx_a = make_rustc_context("/workspace-a/crates/demo/src/lib.rs", "2021");
        let ctx_b = make_rustc_context("/workspace-b/crates/demo/src/lib.rs", "2021");
        let root_a = Path::new("/workspace-a");
        let root_b = Path::new("/workspace-b");

        let ck_a = ctx_a.context_key_with_root(Some(root_a));
        let ck_b = ctx_b.context_key_with_root(Some(root_b));
        assert_eq!(ck_a, ck_b);

        let src_hash = zccache_monocrate::hash::hash_bytes(b"source");
        let dep_hash = zccache_monocrate::hash::hash_bytes(b"dependency");
        let ext_hash = zccache_monocrate::hash::hash_bytes(b"extern");

        let ak_a = compute_rustc_artifact_key_with_root(
            &ck_a,
            &mut [
                (
                    NormalizedPath::from("/workspace-a/crates/demo/src/lib.rs"),
                    src_hash,
                ),
                (
                    NormalizedPath::from("/workspace-a/crates/demo/src/generated.rs"),
                    dep_hash,
                ),
            ],
            &mut [("serde".to_string(), ext_hash)],
            Some(root_a),
        );
        let ak_b = compute_rustc_artifact_key_with_root(
            &ck_b,
            &mut [
                (
                    NormalizedPath::from("/workspace-b/crates/demo/src/generated.rs"),
                    dep_hash,
                ),
                (
                    NormalizedPath::from("/workspace-b/crates/demo/src/lib.rs"),
                    src_hash,
                ),
            ],
            &mut [("serde".to_string(), ext_hash)],
            Some(root_b),
        );

        assert_eq!(
            ak_a, ak_b,
            "source and dependency files under equivalent roots should hash relative to those roots"
        );
    }
}

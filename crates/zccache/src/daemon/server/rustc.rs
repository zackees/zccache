//! Rustc-specific compile context, dep parsing, output enumeration, and RustRemapGate.

use super::*;

/// Build a CompileContext and UserDepFlags from a CacheableCompilation and session info.
/// Result of building a compile context — varies by compiler family.
pub(super) enum BuildContextResult {
    /// C/C++ compilation (GCC, Clang, MSVC).
    Cc {
        ctx: CompileContext,
        dep_flags: UserDepFlags,
    },
    /// Rustc compilation.
    Rustc {
        /// The Rustc-specific context (for context key computation).
        rustc_ctx: Box<crate::depgraph::RustcCompileContext>,
        /// A "compatible" CompileContext for dep_graph storage (has source_file).
        compat_ctx: CompileContext,
        /// Parsed args for extern crate info, output path derivation, etc.
        rustc_args: Box<crate::depgraph::RustcParsedArgs>,
    },
}

pub(super) fn build_compile_context(
    compilation: &crate::compiler::CacheableCompilation,
    cwd: &Path,
    system_includes: &[NormalizedPath],
    client_env: &[(String, String)],
    compiler_hash_cache: &CompilerHashCache,
) -> BuildContextResult {
    if compilation.family == crate::compiler::CompilerFamily::Rustc {
        return build_rustc_compile_context(compilation, cwd, client_env, compiler_hash_cache);
    }

    // Dispatch to the correct parser based on compiler family.
    let parsed = match compilation.family {
        crate::compiler::CompilerFamily::Msvc => {
            crate::depgraph::msvc_args::parse_msvc_args(&compilation.original_args, cwd)
        }
        _ => crate::depgraph::args::parse_gnu_args(&compilation.original_args, cwd),
    };
    let dep_flags = parsed.dep_flags.clone();
    let mut ctx = CompileContext::from_parsed_args(parsed);

    // For multi-file compilations, the parsed source_file might be wrong
    // (it picks the first source from original_args). Override with the
    // correct per-unit source.
    let source_path = if compilation.source_file.is_absolute() {
        compilation.source_file.clone()
    } else {
        cwd.join(&compilation.source_file).into()
    };
    ctx.source_file = source_path;

    // Inject session's system includes
    for path in system_includes {
        if !ctx.include_search.system.contains(path) {
            ctx.include_search.system.push(path.clone());
        }
    }

    BuildContextResult::Cc { ctx, dep_flags }
}

/// Build compile context for a Rustc invocation.
pub(super) fn build_rustc_compile_context(
    compilation: &crate::compiler::CacheableCompilation,
    cwd: &Path,
    client_env: &[(String, String)],
    compiler_hash_cache: &CompilerHashCache,
) -> BuildContextResult {
    let rustc_args = crate::depgraph::parse_rustc_args(&compilation.original_args, cwd);

    // Hash the rustc binary for compiler version identity.
    // Different rustc versions produce different output for the same source.
    let compiler_hash = compiler_hash_cache.get_or_hash(&compilation.compiler);

    let rustc_ctx = crate::depgraph::RustcCompileContext::from_parsed_args(
        &rustc_args,
        client_env,
        compiler_hash,
    );

    // Create a "compatible" CompileContext for dep_graph storage.
    // Only source_file is used by the dep_graph for freshness checks.
    let compat_ctx = CompileContext {
        source_file: rustc_args.source_file.clone(),
        include_search: Default::default(),
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    };

    BuildContextResult::Rustc {
        rustc_ctx: Box::new(rustc_ctx),
        compat_ctx,
        rustc_args: Box::new(rustc_args),
    }
}

/// Scan rustc dependencies after compilation.
///
/// Parses rustc's dep-info file which has multiple rules (one per output target),
/// all sharing the same dependencies. Extracts the unique set of source file deps.
pub(super) fn scan_rustc_deps(
    rustc_args: &crate::depgraph::RustcParsedArgs,
    source_path: &Path,
    cwd: &Path,
) -> crate::depgraph::ScanResult {
    let mut result = if rustc_args.emit_types.iter().any(|t| t == "dep-info") {
        let name = rustc_args.crate_name.as_deref().unwrap_or("unknown");
        let ext_suffix = rustc_args.extra_filename.as_deref().unwrap_or("");
        let dir = rustc_args.out_dir.as_deref().unwrap_or(cwd);
        let depfile_path = dir.join(format!("{name}{ext_suffix}.d"));
        if depfile_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&depfile_path) {
                parse_rustc_depinfo(&content, source_path, cwd)
            } else {
                crate::depgraph::ScanResult {
                    resolved: Vec::new(),
                    unresolved: Vec::new(),
                    has_computed: false,
                }
            }
        } else {
            crate::depgraph::ScanResult {
                resolved: Vec::new(),
                unresolved: Vec::new(),
                has_computed: false,
            }
        }
    } else {
        crate::depgraph::ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: false,
        }
    };

    // Add extern crate files as resolved dependencies.
    // Their content hashes will be part of the artifact key,
    // so changing an extern crate causes a cache miss.
    for ext in &rustc_args.externs {
        let dep_path: NormalizedPath = ext.path.clone().into_path_buf().into();
        if ext.path.exists() && !result.resolved.contains(&dep_path) {
            result.resolved.push(dep_path);
        }
    }

    result
}

/// Parse rustc's multi-rule dep-info format.
///
/// Rustc dep-info files contain multiple rules, one per output target:
/// ```text
/// target1.d: src/lib.rs src/util.rs
/// libtarget1.rlib: src/lib.rs src/util.rs
/// libtarget1.rmeta: src/lib.rs src/util.rs
/// src/lib.rs:
/// src/util.rs:
/// ```
///
/// We extract deps from ALL rules and deduplicate, excluding the source file.
pub(super) fn parse_rustc_depinfo(
    content: &str,
    source_path: &Path,
    cwd: &Path,
) -> crate::depgraph::ScanResult {
    let mut deps = std::collections::HashSet::new();

    for line in content.lines() {
        // Join continuation lines (backslash-newline)
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Find the colon separator (handling Windows drive letters like C:\)
        let colon_pos = if line.len() >= 2
            && line.as_bytes()[1] == b':'
            && line.as_bytes()[0].is_ascii_alphabetic()
        {
            // Skip drive letter colon, find next colon
            line[2..].find(':').map(|p| p + 2)
        } else {
            line.find(':')
        };

        let Some(colon) = colon_pos else { continue };
        let rhs = line[colon + 1..].trim();
        if rhs.is_empty() {
            continue; // "src/lib.rs:" — phony target, skip
        }

        // Split RHS on whitespace, respecting backslash-escaped spaces
        let mut i = 0;
        let bytes = rhs.as_bytes();
        while i < bytes.len() {
            // Skip whitespace
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= bytes.len() {
                break;
            }

            // Collect a token (backslash-space is an escaped space in the path)
            let start = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2; // skip escaped char
                } else {
                    i += 1;
                }
            }
            let raw = &rhs[start..i];
            // Unescape backslash-space
            let token = raw.replace("\\ ", " ");
            deps.insert(token);
        }
    }

    // Resolve paths and filter out the source file
    let source_canonical: NormalizedPath = if source_path.is_absolute() {
        source_path.into()
    } else {
        cwd.join(source_path).into()
    };

    let mut resolved = Vec::new();
    for dep in &deps {
        let dep_path = Path::new(dep);
        let abs = if dep_path.is_absolute() {
            dep_path.to_path_buf()
        } else {
            cwd.join(dep_path)
        };
        // Exclude the source file itself
        if abs == source_canonical {
            continue;
        }
        // Only include files that exist (skip phantom deps)
        if abs.exists() {
            resolved.push(abs.into());
        }
    }
    resolved.sort();

    crate::depgraph::ScanResult {
        resolved,
        unresolved: Vec::new(),
        has_computed: false,
    }
}

pub(super) fn push_unique_output_path(paths: &mut Vec<NormalizedPath>, path: NormalizedPath) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

#[derive(Clone)]
pub(super) struct RustcOutputFile {
    pub(super) name: String,
    pub(super) path: NormalizedPath,
    pub(super) size: u64,
}

pub(super) fn rustc_expected_output_paths(
    rustc_args: &crate::depgraph::RustcParsedArgs,
    primary_output_path: &Path,
    cwd: &Path,
) -> Vec<NormalizedPath> {
    let mut paths = vec![NormalizedPath::new(primary_output_path)];
    let crate_name = rustc_args.crate_name.as_deref().unwrap_or("unknown");
    let ext_suffix = rustc_args.extra_filename.as_deref().unwrap_or("");
    let dir = rustc_args.out_dir.as_deref().unwrap_or(cwd);

    for emit_type in &rustc_args.emit_types {
        let candidate = match emit_type.as_str() {
            "metadata" => Some(dir.join(format!("lib{crate_name}{ext_suffix}.rmeta"))),
            "link" => Some(dir.join(format!("lib{crate_name}{ext_suffix}.rlib"))),
            "dep-info" => Some(dir.join(format!("{crate_name}{ext_suffix}.d"))),
            "obj" => Some(dir.join(format!("{crate_name}{ext_suffix}.o"))),
            "asm" => Some(dir.join(format!("{crate_name}{ext_suffix}.s"))),
            "llvm-ir" => Some(dir.join(format!("{crate_name}{ext_suffix}.ll"))),
            "mir" => Some(dir.join(format!("{crate_name}{ext_suffix}.mir"))),
            _ => None,
        };
        if let Some(path) = candidate {
            push_unique_output_path(&mut paths, path.into());
        }
    }

    paths
}

/// Collect output file metadata from a rustc compilation without reading bytes.
pub(super) fn collect_rustc_output_files(
    rustc_args: &crate::depgraph::RustcParsedArgs,
    primary_output_path: &Path,
    cwd: &Path,
) -> Vec<RustcOutputFile> {
    let Ok(primary_meta) = std::fs::metadata(primary_output_path) else {
        return Vec::new();
    };
    let primary_name = primary_output_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let mut outputs = vec![RustcOutputFile {
        name: primary_name,
        path: NormalizedPath::new(primary_output_path),
        size: primary_meta.len(),
    }];

    // Find additional outputs based on --emit types
    let crate_name = rustc_args.crate_name.as_deref().unwrap_or("unknown");
    let ext_suffix = rustc_args.extra_filename.as_deref().unwrap_or("");
    let dir = rustc_args.out_dir.as_deref().unwrap_or(cwd);

    for emit_type in &rustc_args.emit_types {
        let candidate = match emit_type.as_str() {
            "metadata" => {
                let path = dir.join(format!("lib{crate_name}{ext_suffix}.rmeta"));
                if path != primary_output_path && path.exists() {
                    Some(path)
                } else {
                    None
                }
            }
            "link" => {
                // Could be rlib or staticlib
                let rlib = dir.join(format!("lib{crate_name}{ext_suffix}.rlib"));
                let staticlib = dir.join(format!("lib{crate_name}{ext_suffix}.a"));
                if rlib != primary_output_path && rlib.exists() {
                    Some(rlib)
                } else if staticlib != primary_output_path && staticlib.exists() {
                    Some(staticlib)
                } else {
                    None
                }
            }
            "dep-info" => {
                let path = dir.join(format!("{crate_name}{ext_suffix}.d"));
                if path.exists() {
                    Some(path)
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(path) = candidate {
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            // Avoid duplicates
            if !outputs.iter().any(|existing| existing.name == name) {
                if let Ok(meta) = std::fs::metadata(&path) {
                    if meta.is_file() {
                        outputs.push(RustcOutputFile {
                            name,
                            path: path.into(),
                            size: meta.len(),
                        });
                    }
                }
            }
        }
    }

    outputs
}
pub(super) fn rust_remap_value_matches_old(value: &str, old: &Path) -> bool {
    let Some((existing_old, _)) = value.split_once('=') else {
        return false;
    };
    let existing_old = Path::new(existing_old);
    existing_old.is_absolute() && same_key_path(existing_old, old)
}

pub(super) fn rust_remap_values_have_old<'a>(
    values: impl IntoIterator<Item = &'a String>,
    old: &Path,
) -> bool {
    values
        .into_iter()
        .any(|value| rust_remap_value_matches_old(value, old))
}

pub(super) fn rust_args_have_remap_for_old(args: &[String], old: &Path) -> bool {
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--remap-path-prefix" {
            if let Some(value) = args.get(i + 1) {
                if rust_remap_value_matches_old(value, old) {
                    return true;
                }
            }
            i += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--remap-path-prefix=") {
            if rust_remap_value_matches_old(value, old) {
                return true;
            }
        }
        i += 1;
    }
    false
}

pub(super) fn compiler_is_rustc_like(compiler_path: &Path) -> bool {
    crate::compiler::detect_family(&compiler_path.to_string_lossy())
        == crate::compiler::CompilerFamily::Rustc
}

pub(super) fn rustc_request_key_root(
    args: &[String],
    worktree_root: Option<&NormalizedPath>,
) -> Option<NormalizedPath> {
    let root = worktree_root?;
    rust_args_have_remap_for_old(args, root.as_path()).then(|| root.clone())
}

pub(super) fn rustc_context_key_root(
    remap_path_prefixes: &[String],
    worktree_root: Option<&NormalizedPath>,
) -> Option<NormalizedPath> {
    let root = worktree_root?;
    rust_remap_values_have_old(remap_path_prefixes.iter(), root.as_path()).then(|| root.clone())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RustRemapGate {
    Ok,
    Missing,
    OldOutsideRoot,
    Malformed,
}

impl RustRemapGate {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            RustRemapGate::Ok => "rust_remap_gate_ok",
            RustRemapGate::Missing => "rust_remap_missing",
            RustRemapGate::OldOutsideRoot => "rust_remap_old_outside_root",
            RustRemapGate::Malformed => "rust_remap_malformed",
        }
    }
}

pub(super) fn rust_remap_gate(
    remap_path_prefixes: &[String],
    worktree_root: Option<&NormalizedPath>,
) -> RustRemapGate {
    let Some(root) = worktree_root else {
        return RustRemapGate::Missing;
    };
    let root_key = crate::core::path::normalize_for_key(root.as_path());
    let root_child_prefix = format!("{root_key}/");
    let mut saw_malformed = false;
    let mut saw_external = false;

    for value in remap_path_prefixes {
        let Some((old, _new)) = value.split_once('=') else {
            saw_malformed = true;
            continue;
        };
        let old_path = Path::new(old);
        if !old_path.is_absolute() {
            saw_malformed = true;
            continue;
        }
        let old_key = crate::core::path::normalize_for_key(old_path);
        if old_key == root_key {
            return RustRemapGate::Ok;
        }
        if !old_key.starts_with(&root_child_prefix) {
            saw_external = true;
        }
    }

    if saw_malformed {
        RustRemapGate::Malformed
    } else if saw_external {
        RustRemapGate::OldOutsideRoot
    } else {
        RustRemapGate::Missing
    }
}

//! Rustc-specific compile context, dep parsing, output enumeration, and RustRemapGate.

use super::*;

/// Fallback compiler-identity hash used only when `CompilerHashCache`
/// cannot even `stat` the compiler binary (the initial `std::fs::metadata`
/// call in `get_or_hash_with[_async]` fails) — a pathological case since
/// the compiler was already spawned to reach this code path. `compiler_hash`
/// is a required (non-`Option`) field on `CompileContext` /
/// `RustcCompileContext` (issue #1166), so callers need a concrete value
/// even in this edge case; a fixed sentinel keeps the resulting context key
/// well-defined and deterministic rather than panicking.
pub(super) const COMPILER_HASH_UNAVAILABLE: ContentHash = ContentHash::from_bytes([0u8; 32]);

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

    // Compiler identity for the cache key (issue #1166): an in-place
    // toolchain upgrade (same path, new clang/gcc/cl.exe binary content)
    // must not reuse a stale cache key. Mirrors the rustc branch's `-vV`
    // probe (issue #517) via `hash_cc_identity` (`--version`).
    let compiler_hash = compiler_hash_cache
        .get_or_hash_with(&compilation.compiler, hash_cc_identity)
        .unwrap_or(COMPILER_HASH_UNAVAILABLE);

    build_cc_compile_context(compilation, cwd, system_includes, client_env, compiler_hash)
}

/// Shared by the sync and async C/C++ context builders once the compiler
/// identity hash has been resolved (issue #1166).
fn build_cc_compile_context(
    compilation: &crate::compiler::CacheableCompilation,
    cwd: &Path,
    system_includes: &[NormalizedPath],
    client_env: &[(String, String)],
    compiler_hash: ContentHash,
) -> BuildContextResult {
    // Dispatch to the correct parser based on compiler family.
    let parsed = match compilation.family {
        crate::compiler::CompilerFamily::Msvc => {
            crate::depgraph::msvc_args::parse_msvc_args(&compilation.original_args, cwd)
        }
        _ => crate::depgraph::args::parse_gnu_args(&compilation.original_args, cwd),
    };
    let dep_flags = parsed.dep_flags.clone();
    let mut ctx = CompileContext::from_parsed_args(parsed, compiler_hash);
    ctx.flags
        .extend(msvc_env_key_flags(compilation.family, client_env));
    ctx.flags.sort();

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

pub(super) async fn build_compile_context_async(
    compilation: &crate::compiler::CacheableCompilation,
    cwd: &Path,
    system_includes: &[NormalizedPath],
    client_env: &[(String, String)],
    compiler_hash_cache: &CompilerHashCache,
) -> BuildContextResult {
    if compilation.family == crate::compiler::CompilerFamily::Rustc {
        return build_rustc_compile_context_async(
            compilation,
            cwd,
            client_env,
            compiler_hash_cache,
        )
        .await;
    }

    // Async sibling of the sync Cc branch above: hash the compiler binary
    // off the blocking-spawn path so the tokio worker thread is not
    // parked on the `--version` probe (issue #1166; mirrors the rustc
    // async branch's use of `hash_rustc_identity_async`).
    let compiler_hash = compiler_hash_cache
        .get_or_hash_with_async(&compilation.compiler, hash_cc_identity_async)
        .await
        .unwrap_or(COMPILER_HASH_UNAVAILABLE);

    build_cc_compile_context(compilation, cwd, system_includes, client_env, compiler_hash)
}

/// Build compile context for a Rustc invocation.
pub(super) fn build_rustc_compile_context(
    compilation: &crate::compiler::CacheableCompilation,
    cwd: &Path,
    client_env: &[(String, String)],
    compiler_hash_cache: &CompilerHashCache,
) -> BuildContextResult {
    let mut rustc_args = crate::depgraph::parse_rustc_args(rustc_args(compilation), cwd);
    let compiler_identity_path = rustc_identity_path(compilation, cwd);

    // Compiler identity for the cache key. Different rustc versions
    // produce different output for the same source, so the identity
    // hash must vary with the toolchain build.
    //
    // Issue #517: prefer `rustc -vV` output (~10 ms spawn) over a full
    // blake3 over the ~150 MB binary (~50-60 ms on Linux). The cache
    // is still keyed by the binary's (path, mtime, size); only the
    // identity bytes that get hashed change. A probe fallback is safe for
    // this request but deliberately not memoized, so a transient failure
    // cannot persist a second toolchain identity flavor (#1167).
    let compiler_hash = compiler_hash_cache
        .get_or_hash_rustc_identity(compiler_identity_path.as_path())
        .unwrap_or(COMPILER_HASH_UNAVAILABLE);
    if let Some(linker) = rustc_args
        .linker
        .clone()
        .filter(|_| is_dylint_cdylib_args(&rustc_args))
    {
        let linker_hash = compiler_hash_cache
            .get_or_hash_with(&linker, hash_cc_identity)
            .unwrap_or(COMPILER_HASH_UNAVAILABLE);
        add_dylint_linker_key_material(&mut rustc_args, linker_hash);
    }

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
        compiler_hash,
    };

    BuildContextResult::Rustc {
        rustc_ctx: Box::new(rustc_ctx),
        compat_ctx,
        rustc_args: Box::new(rustc_args),
    }
}

pub(super) async fn build_rustc_compile_context_async(
    compilation: &crate::compiler::CacheableCompilation,
    cwd: &Path,
    client_env: &[(String, String)],
    compiler_hash_cache: &CompilerHashCache,
) -> BuildContextResult {
    let mut rustc_args = crate::depgraph::parse_rustc_args(rustc_args(compilation), cwd);
    let compiler_identity_path = rustc_identity_path(compilation, cwd);

    let compiler_hash = compiler_hash_cache
        .get_or_hash_rustc_identity_async(compiler_identity_path.as_path())
        .await
        .unwrap_or(COMPILER_HASH_UNAVAILABLE);
    if let Some(linker) = rustc_args
        .linker
        .clone()
        .filter(|_| is_dylint_cdylib_args(&rustc_args))
    {
        let linker_hash = compiler_hash_cache
            .get_or_hash_with_async(&linker, hash_cc_identity_async)
            .await
            .unwrap_or(COMPILER_HASH_UNAVAILABLE);
        add_dylint_linker_key_material(&mut rustc_args, linker_hash);
    }

    let rustc_ctx = crate::depgraph::RustcCompileContext::from_parsed_args(
        &rustc_args,
        client_env,
        compiler_hash,
    );

    let compat_ctx = CompileContext {
        source_file: rustc_args.source_file.clone(),
        include_search: Default::default(),
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
        compiler_hash,
    };

    BuildContextResult::Rustc {
        rustc_ctx: Box::new(rustc_ctx),
        compat_ctx,
        rustc_args: Box::new(rustc_args),
    }
}

fn is_dylint_cdylib_args(args: &crate::depgraph::RustcParsedArgs) -> bool {
    !crate::platform::host::is_windows()
        && args.crate_types == ["cdylib"]
        && args.target.is_none()
        && args.extra_filename.as_deref().is_none_or(str::is_empty)
        && args.linker.as_ref().is_some_and(|linker| {
            linker
                .file_stem()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|stem| stem.eq_ignore_ascii_case("dylint-link"))
        })
        && args.out_dir.as_ref().is_some_and(|out_dir| {
            let components: Vec<_> = out_dir.components().collect();
            components.windows(2).any(|pair| {
                pair[0].as_os_str() == std::ffi::OsStr::new("dylint")
                    && pair[1].as_os_str() == std::ffi::OsStr::new("libraries")
            })
        })
}

fn add_dylint_linker_key_material(
    args: &mut crate::depgraph::RustcParsedArgs,
    linker_hash: ContentHash,
) {
    args.codegen_flags
        .push(format!("dylint-linker-hash={linker_hash}"));
    args.codegen_flags.extend(args.linker_args.clone());
    args.codegen_flags.sort();
}

fn rustc_args(compilation: &crate::compiler::CacheableCompilation) -> &[String] {
    crate::compiler::dylint_inner_rustc_args(
        compilation.compiler.to_str().unwrap_or(""),
        &compilation.original_args,
    )
    .ok()
    .flatten()
    .map_or(compilation.original_args.as_ref(), |(_, args)| args)
}

fn rustc_identity_path(
    compilation: &crate::compiler::CacheableCompilation,
    cwd: &Path,
) -> NormalizedPath {
    let inner = crate::compiler::dylint_inner_rustc_args(
        compilation.compiler.to_str().unwrap_or(""),
        &compilation.original_args,
    )
    .ok()
    .flatten()
    .map(|(inner, _)| Path::new(inner))
    .unwrap_or(compilation.compiler.as_path());
    if inner.is_absolute() {
        NormalizedPath::new(inner)
    } else {
        NormalizedPath::new(cwd).join(inner)
    }
}

/// Result of scanning rustc's dep-info after a compile: the file
/// dependencies plus the env-dep variable names rustc recorded
/// (zccache#1021).
pub(super) struct RustcDepScan {
    pub(super) scan: crate::depgraph::ScanResult,
    /// Env variable names from `# env-dep:NAME[=value]` lines — every
    /// `env!()`/`option_env!()` the crate read at compile time. Values
    /// are re-resolved from the request env; only the names are taken
    /// from dep-info.
    pub(super) env_dep_names: Vec<String>,
}

fn empty_scan() -> crate::depgraph::ScanResult {
    crate::depgraph::ScanResult {
        resolved: Vec::new(),
        unresolved: Vec::new(),
        has_computed: false,
    }
}

/// Scan rustc dependencies after compilation.
///
/// Parses rustc's dep-info file which has multiple rules (one per output target),
/// all sharing the same dependencies. Extracts the unique set of source file deps
/// and the `# env-dep:` variable names (zccache#1021).
/// `--extern` crate files are tracked separately by the dependency graph so
/// their content, but not target-dir path prefix, participates in artifact keys.
pub(super) fn scan_rustc_deps(
    rustc_args: &crate::depgraph::RustcParsedArgs,
    source_path: &Path,
    cwd: &Path,
) -> RustcDepScan {
    if rustc_args.emit_types.iter().any(|t| t == "dep-info") {
        if let Some(depfile_path) = rustc_depfile_output_path(rustc_args, cwd) {
            if depfile_path.exists() {
                if let Ok(content) = std::fs::read_to_string(&depfile_path) {
                    return parse_rustc_depinfo(&content, source_path, cwd);
                }
            }
        }
    }
    RustcDepScan {
        scan: empty_scan(),
        env_dep_names: Vec::new(),
    }
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
/// We extract deps from ALL rules and deduplicate, excluding the source
/// file. `# env-dep:NAME[=value]` comment lines are collected as env-dep
/// NAMES (zccache#1021) — previously they fell through the rule parser
/// and were silently discarded by the exists() filter.
pub(super) fn parse_rustc_depinfo(content: &str, source_path: &Path, cwd: &Path) -> RustcDepScan {
    let mut deps = std::collections::HashSet::new();
    let mut env_dep_names: Vec<String> = Vec::new();

    for line in content.lines() {
        // Join continuation lines (backslash-newline)
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // `# env-dep:NAME[=value]` — rustc records every env!()/
        // option_env!() read. Take the NAME (values are re-resolved from
        // the request env at key time; rustc escapes values in-place so
        // splitting on the first '=' is safe for the name).
        if let Some(rest) = line.strip_prefix("# env-dep:") {
            let name = rest.split('=').next().unwrap_or(rest).trim();
            if !name.is_empty() && !env_dep_names.iter().any(|n| n == name) {
                env_dep_names.push(name.to_string());
            }
            continue;
        }
        // Any other comment line — never a dep rule.
        if line.starts_with('#') {
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
    env_dep_names.sort();

    RustcDepScan {
        scan: crate::depgraph::ScanResult {
            resolved,
            unresolved: Vec::new(),
            has_computed: false,
        },
        env_dep_names,
    }
}

pub(super) fn push_unique_output_path(paths: &mut Vec<NormalizedPath>, path: NormalizedPath) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

pub(super) fn rustc_depfile_output_path(
    rustc_args: &crate::depgraph::RustcParsedArgs,
    cwd: &Path,
) -> Option<NormalizedPath> {
    if !rustc_args.emit_types.iter().any(|kind| kind == "dep-info") {
        return None;
    }
    if let Some((_, path)) = rustc_args
        .explicit_emit_paths
        .iter()
        .find(|(kind, _)| kind == "dep-info")
    {
        return Some(path.clone());
    }
    let crate_name = rustc_args.crate_name.as_deref().unwrap_or("unknown");
    let extra_filename = rustc_args.extra_filename.as_deref().unwrap_or("");
    let output_dir = rustc_args.out_dir.as_deref().unwrap_or(cwd);
    Some(
        output_dir
            .join(format!("{crate_name}{extra_filename}.d"))
            .into(),
    )
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
    client_env: Option<&[(String, String)]>,
) -> Vec<NormalizedPath> {
    let explicit_link = rustc_args
        .explicit_emit_paths
        .iter()
        .find(|(kind, _)| kind == "link")
        .map(|(_, path)| path.clone());
    let mut paths = vec![explicit_link.unwrap_or_else(|| NormalizedPath::new(primary_output_path))];
    let crate_name = rustc_args.crate_name.as_deref().unwrap_or("unknown");
    let ext_suffix = rustc_args.extra_filename.as_deref().unwrap_or("");
    let dir = rustc_args.out_dir.as_deref().unwrap_or(cwd);

    for emit_type in &rustc_args.emit_types {
        if rustc_args
            .explicit_emit_paths
            .iter()
            .any(|(kind, _)| kind == emit_type)
        {
            continue;
        }
        let candidate = match emit_type.as_str() {
            "metadata" => Some(dir.join(format!("lib{crate_name}{ext_suffix}.rmeta"))),
            // The parser's primary output is authoritative for `link`: it
            // already accounts for bin, staticlib, proc-macro, target, and
            // explicit `--emit=link=...` naming. Inferring an rlib here would
            // create a false required output for those crate types.
            "link" => None,
            "dep-info" => Some(dir.join(format!("{crate_name}{ext_suffix}.d"))),
            "obj" => Some(dir.join(format!("{crate_name}{ext_suffix}.o"))),
            "asm" => Some(dir.join(format!("{crate_name}{ext_suffix}.s"))),
            "llvm-ir" => Some(dir.join(format!("{crate_name}{ext_suffix}.ll"))),
            "llvm-bc" | "bitcode" => Some(dir.join(format!("{crate_name}{ext_suffix}.bc"))),
            "mir" => Some(dir.join(format!("{crate_name}{ext_suffix}.mir"))),
            _ => None,
        };
        if let Some(path) = candidate {
            push_unique_output_path(&mut paths, path.into());
        }
    }

    for (kind, explicit_path) in &rustc_args.explicit_emit_paths {
        let replacement = paths.iter().position(|path| match kind.as_str() {
            "metadata" => path.extension().and_then(|ext| ext.to_str()) == Some("rmeta"),
            "link" => matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("rlib" | "a" | "exe") | None
            ),
            "dep-info" => path.extension().and_then(|ext| ext.to_str()) == Some("d"),
            "obj" => path.extension().and_then(|ext| ext.to_str()) == Some("o"),
            "asm" => path.extension().and_then(|ext| ext.to_str()) == Some("s"),
            "llvm-ir" => path.extension().and_then(|ext| ext.to_str()) == Some("ll"),
            "llvm-bc" | "bitcode" => path.extension().and_then(|ext| ext.to_str()) == Some("bc"),
            "mir" => path.extension().and_then(|ext| ext.to_str()) == Some("mir"),
            _ => false,
        });
        if let Some(index) = replacement {
            paths[index] = explicit_path.clone();
        } else {
            push_unique_output_path(&mut paths, explicit_path.clone());
        }
    }

    if let Some(sidecar) =
        dylint_library_sidecar_output_path(rustc_args, primary_output_path, cwd, client_env)
    {
        push_unique_output_path(&mut paths, sidecar);
    }

    // soldr#2148. Declaring it is what gets it redirected into the staging
    // directory alongside the image, captured on a miss, and replayed on a
    // hit -- as part of the same cache entry, so the pair cannot desynchronise.
    //
    // soldr#2347: only when the *target* is MSVC. A windows-gnu image is
    // linked by mingw, which keeps DWARF in the image and never writes a
    // `.pdb` — a staged plan that declares one then hard-fails at
    // materialization when the file does not exist, killing every
    // Linux-hosted `--target x86_64-pc-windows-gnu` compile of a linked
    // image (the non-staged enumeration below already probes the
    // filesystem and was naturally immune).
    if msvc_target_writes_pdb(rustc_args) {
        if let Some(pdb) = msvc_pdb_sidecar_output_path(primary_output_path) {
            push_unique_output_path(&mut paths, pdb);
        }
    }

    paths
}

fn dylint_library_sidecar_output_path(
    rustc_args: &crate::depgraph::RustcParsedArgs,
    primary_output_path: &Path,
    cwd: &Path,
    client_env: Option<&[(String, String)]>,
) -> Option<NormalizedPath> {
    if crate::platform::host::is_windows() || rustc_args.crate_types != ["cdylib"] {
        return None;
    }
    let linker_stem = rustc_args.linker.as_ref()?.file_stem()?.to_str()?;
    if !linker_stem.eq_ignore_ascii_case("dylint-link") {
        return None;
    }
    let out_dir = rustc_args.out_dir.as_ref()?;
    let components: Vec<_> = out_dir.components().collect();
    if !components.windows(2).any(|pair| {
        pair[0].as_os_str() == std::ffi::OsStr::new("dylint")
            && pair[1].as_os_str() == std::ffi::OsStr::new("libraries")
    }) {
        return None;
    }
    let env = client_env?;
    let env_value = |name: &str| {
        env.iter()
            .find_map(|(key, value)| (key == name).then_some(value.as_str()))
    };
    let package_name = env_value("CARGO_PKG_NAME")?.replace('-', "_");
    let crate_name = rustc_args.crate_name.as_deref()?;
    if package_name != crate_name {
        return None;
    }
    let toolchain = env_value("RUSTUP_TOOLCHAIN")?.trim();
    if toolchain.is_empty() {
        return None;
    }

    let primary = if primary_output_path.is_absolute() {
        NormalizedPath::new(primary_output_path)
    } else {
        NormalizedPath::new(cwd).join(primary_output_path)
    };
    let parent = primary.parent()?;
    let sidecar_dir = if parent.file_name() == Some(std::ffi::OsStr::new("deps")) {
        parent.parent()?
    } else {
        parent
    };
    let suffix = if crate::platform::host::is_macos() {
        ".dylib"
    } else {
        ".so"
    };
    Some(
        sidecar_dir
            .join(format!("lib{crate_name}@{toolchain}{suffix}"))
            .into(),
    )
}

/// `<primary>.pdb` beside a linked Windows image.
///
/// MSVC keeps debug info in a separate file named after the image, so the
/// `.pdb` is a real product of the link step that rustc drives. Nothing in the
/// rustc output model knew about it, so it was never staged, never stored and
/// never replayed: a cached build produced the `.exe` alone and the binary's
/// `RSDS` record pointed at a file that was not there (soldr#2148).
///
/// Returns `None` for outputs that never have one -- rlib, rmeta, staticlib --
/// so nothing extra is declared for the common case. A declared output that
/// the compiler does not produce (debuginfo off, or a non-MSVC target that
/// still emits an `.exe`) is filtered out at collection time rather than
/// failing the compile, so this does not need to predict debuginfo settings.
/// Whether the compile's target links with MSVC and can therefore
/// produce a `.pdb` beside the image (soldr#2347). An explicit
/// `--target` decides directly; with no `--target`, the compile is
/// host-native and only an MSVC host writes pdbs.
pub(super) fn msvc_target_writes_pdb(rustc_args: &crate::depgraph::RustcParsedArgs) -> bool {
    match rustc_args.target.as_deref() {
        Some(triple) => triple.ends_with("-pc-windows-msvc"),
        None => cfg!(all(target_os = "windows", target_env = "msvc")),
    }
}

pub(super) fn msvc_pdb_sidecar_output_path(primary_output_path: &Path) -> Option<NormalizedPath> {
    let extension = primary_output_path.extension()?.to_str()?;
    if !extension.eq_ignore_ascii_case("exe") && !extension.eq_ignore_ascii_case("dll") {
        return None;
    }
    Some(NormalizedPath::new(
        primary_output_path.with_extension("pdb"),
    ))
}

pub(super) fn dylint_cdylib_has_complete_output_identity(
    rustc_args: &crate::depgraph::RustcParsedArgs,
    primary_output_path: &Path,
    cwd: &Path,
    client_env: Option<&[(String, String)]>,
) -> bool {
    rustc_args.crate_types != ["cdylib"]
        || dylint_library_sidecar_output_path(rustc_args, primary_output_path, cwd, client_env)
            .is_some()
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

    for (_, path) in &rustc_args.explicit_emit_paths {
        if path != primary_output_path && path.exists() {
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if !outputs.iter().any(|existing| existing.name == name) {
                if let Ok(meta) = std::fs::metadata(path) {
                    if meta.is_file() {
                        outputs.push(RustcOutputFile {
                            name,
                            path: path.clone(),
                            size: meta.len(),
                        });
                    }
                }
            }
        }
    }

    // soldr#2148. This is the enumeration used when the staged plan is not
    // enabled; `rustc_expected_output_paths` covers the staged one. Both need
    // it, or the `.pdb` survives in one configuration and vanishes in the
    // other -- which is worse than losing it consistently, because it makes
    // the bug look intermittent.
    if let Some(pdb) = msvc_pdb_sidecar_output_path(primary_output_path) {
        if let Ok(meta) = std::fs::metadata(&pdb) {
            if meta.is_file() {
                let name = pdb
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                if !outputs.iter().any(|existing| existing.name == name) {
                    outputs.push(RustcOutputFile {
                        name,
                        path: pdb,
                        size: meta.len(),
                    });
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

#[cfg(test)]
mod dylint_sidecar_tests {
    use super::*;

    #[test]
    fn perf_dylint_cdylib_models_toolchain_sidecar_as_complete_output_set() {
        if crate::platform::host::is_windows() {
            return;
        }
        let cwd = Path::new("/repo");
        let out_dir = "/repo/target/dylint/libraries/nightly/release/deps";
        let args = vec![
            "--crate-name=lint".to_string(),
            "--crate-type=cdylib".to_string(),
            "--emit=link".to_string(),
            format!("--out-dir={out_dir}"),
            "-Clinker=/tools/dylint-link".to_string(),
            "src/lib.rs".to_string(),
        ];
        let parsed = crate::depgraph::parse_rustc_args(&args, cwd);
        let extension = if crate::platform::host::is_macos() {
            "dylib"
        } else {
            "so"
        };
        let primary = Path::new(out_dir).join(format!("liblint.{extension}"));
        let env = vec![
            ("CARGO_PKG_NAME".to_string(), "lint".to_string()),
            (
                "RUSTUP_TOOLCHAIN".to_string(),
                "nightly-2026-01-18-x86_64-unknown-linux-gnu".to_string(),
            ),
        ];

        let outputs = rustc_expected_output_paths(&parsed, &primary, cwd, Some(&env));
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0], NormalizedPath::new(&primary));
        assert!(outputs[1].ends_with(format!(
            "release/liblint@nightly-2026-01-18-x86_64-unknown-linux-gnu.{extension}"
        )));

        let without_identity = rustc_expected_output_paths(&parsed, &primary, cwd, None);
        assert_eq!(without_identity, vec![NormalizedPath::new(primary)]);
    }
}

/// soldr#2148. Deliberately NOT `cfg(not(target_os = "windows"))` like the
/// dylint sidecar tests above: `msvc_pdb_sidecar_output_path` is pure path
/// manipulation, and Windows is precisely where its absence was the bug.
#[cfg(test)]
mod pdb_sidecar_tests {
    use super::*;

    #[test]
    fn msvc_pdb_is_declared_for_linked_images_only() {
        // soldr#2148: a cached build produced the .exe without its .pdb, so
        // crash dumps resolved to `module+0xNNNN`. The pdb was never in the
        // output model, so it was never staged, stored or replayed.
        for image in ["app.exe", "plugin.dll", "APP.EXE"] {
            let pdb = msvc_pdb_sidecar_output_path(Path::new(image))
                .unwrap_or_else(|| panic!("{image} should declare a pdb"));
            assert_eq!(
                pdb.extension().and_then(|e| e.to_str()),
                Some("pdb"),
                "{image} -> {pdb:?}"
            );
        }

        // Artifacts that never have one. Declaring a pdb for these would be
        // harmless (missing outputs are filtered at collection) but it would
        // also be a lie about what the compile produces.
        for other in ["libfoo.rlib", "libfoo.rmeta", "libfoo.a", "foo.d", "noext"] {
            assert!(
                msvc_pdb_sidecar_output_path(Path::new(other)).is_none(),
                "{other} must not declare a pdb"
            );
        }
    }

    /// soldr#2347: the pdb declaration is target-aware. A windows-gnu
    /// image is linked by mingw (DWARF in the image, no pdb ever); the
    /// staged plan hard-fails materialization on a declared output that
    /// never appears, which killed every Linux-hosted
    /// `--target x86_64-pc-windows-gnu` linked-image compile.
    #[test]
    fn pdb_declaration_is_msvc_target_only() {
        let cwd = Path::new("/repo");
        let base = |target: Option<&str>| {
            let mut args = vec![
                "--crate-name=wg".to_string(),
                "--crate-type=bin".to_string(),
                "--emit=link".to_string(),
                "--out-dir=/repo/target/deps".to_string(),
                "src/main.rs".to_string(),
            ];
            if let Some(target) = target {
                args.push(format!("--target={target}"));
            }
            crate::depgraph::parse_rustc_args(&args, cwd)
        };

        let msvc = base(Some("x86_64-pc-windows-msvc"));
        assert!(msvc_target_writes_pdb(&msvc));
        let primary = Path::new("/repo/target/deps/wg.exe");
        let declared = rustc_expected_output_paths(&msvc, primary, cwd, None);
        assert!(
            declared.iter().any(|p| p.extension() == Some("pdb".as_ref())),
            "msvc target must declare the pdb sidecar: {declared:?}"
        );

        let gnu = base(Some("x86_64-pc-windows-gnu"));
        assert!(!msvc_target_writes_pdb(&gnu));
        let declared = rustc_expected_output_paths(&gnu, primary, cwd, None);
        assert!(
            !declared.iter().any(|p| p.extension() == Some("pdb".as_ref())),
            "windows-gnu never writes a pdb; declaring one hard-fails the              staged materialization (soldr#2347): {declared:?}"
        );

        let aarch_gnu = base(Some("aarch64-pc-windows-gnullvm"));
        assert!(!msvc_target_writes_pdb(&aarch_gnu));
    }
}

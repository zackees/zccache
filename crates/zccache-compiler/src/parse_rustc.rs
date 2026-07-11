//! Rustc-invocation parsing.
//!
//! Rustc has a completely different invocation model from C/C++ compilers:
//! crate types, `--emit=` mixed types, host-side proc-macro dylibs, etc.

use std::sync::Arc;
use zccache_core::NormalizedPath;

use super::{CacheableCompilation, CompilerFamily, ParsedInvocation};

/// Cacheable rustc crate types.
///
/// - `lib`, `rlib`, `staticlib`: archive outputs, no system linker.
/// - `proc-macro`: a host-side dylib loaded by rustc at compile time.
///   The output is a single deterministic shared library; sccache
///   caches the same set. The artifact key already covers source
///   content, deps, and compiler identity, so the safety contract
///   is the same as any other rustc invocation.
///   Crate types zccache caches (zccache#1021 documents the exclusions):
///   `dylib` and `cdylib` are deliberately NOT cacheable — dynamic
///   libraries embed platform linker state (soname/install-name, import
///   libs) that the artifact store does not model, so PyO3/maturin
///   `cdylib` final artifacts recompile every time while their rlib deps
///   still hit.
const RUSTC_CACHEABLE_CRATE_TYPES: &[&str] = &["lib", "rlib", "staticlib", "proc-macro", "bin"];

/// Host dynamic-library file-name pattern for proc-macros, matching
/// rustc's output naming. Linux/macOS use the `lib` prefix; Windows
/// doesn't.
fn rustc_proc_macro_filename(crate_name: &str, extra: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{crate_name}{extra}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{crate_name}{extra}.dylib")
    } else {
        format!("lib{crate_name}{extra}.so")
    }
}

/// Host executable file-name pattern for `--crate-type bin`. Windows
/// adds `.exe`; unix has no extension.
fn rustc_bin_filename(crate_name: &str, extra: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{crate_name}{extra}.exe")
    } else {
        format!("{crate_name}{extra}")
    }
}

/// Rustc flags that take a following argument (value in next argv element).
const RUSTC_FLAGS_WITH_VALUE: &[&str] = &[
    "--edition",
    "--crate-type",
    "--crate-name",
    "--emit",
    "--out-dir",
    "--target",
    "--cap-lints",
    "--extern",
    "--error-format",
    "--json",
    "--color",
    "--diagnostic-width",
    "--sysroot",
    "--cfg",
    "--check-cfg",
    "-o",
    "-L",
    "-C",
    "-A",
    "-W",
    "-D",
    "-F",
    "--codegen",
    "--remap-path-prefix",
    "--env-set",
];

/// Parse a rustc invocation to determine cacheability.
///
/// Cacheable: `--crate-type` is `lib`, `rlib`, `staticlib`, `proc-macro`, or `bin`.
/// Non-cacheable: `dylib`, `cdylib`.
pub(crate) fn parse_rustc_invocation(compiler: &str, args: &[String]) -> ParsedInvocation {
    let mut crate_types: Vec<String> = Vec::new();
    let mut source_file: Option<String> = None;
    let mut output_file: Option<String> = None;
    let mut out_dir: Option<String> = None;
    let mut crate_name: Option<String> = None;
    let mut extra_filename: Option<String> = None;
    let mut emit_types: Vec<String> = Vec::new();
    let mut explicit_link_output: Option<String> = None;
    let mut explicit_output: Option<String> = None;
    let mut unknown_flags: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        // --crate-type <type> or --crate-type=<type>
        // Rustc accepts comma-separated types: --crate-type lib,rlib
        if arg == "--crate-type" {
            if let Some(next) = args.get(i + 1) {
                crate_types.extend(next.split(',').map(|s| s.to_string()));
                i += 2;
                continue;
            }
        } else if let Some(val) = arg.strip_prefix("--crate-type=") {
            crate_types.extend(val.split(',').map(|s| s.to_string()));
            i += 1;
            continue;
        }

        // --crate-name <name> or --crate-name=<name>
        if arg == "--crate-name" {
            if let Some(next) = args.get(i + 1) {
                crate_name = Some(next.clone());
                i += 2;
                continue;
            }
        } else if let Some(val) = arg.strip_prefix("--crate-name=") {
            crate_name = Some(val.to_string());
            i += 1;
            continue;
        }

        // --emit <types> or --emit=<types>
        if arg == "--emit" {
            if let Some(next) = args.get(i + 1) {
                emit_types.extend(next.split(',').map(|s| {
                    // Handle --emit=dep-info=path form
                    s.split('=').next().unwrap_or(s).to_string()
                }));
                for part in next.split(',') {
                    if let Some((kind, path)) = part.split_once('=') {
                        if kind == "link" && path != "-" {
                            explicit_link_output = Some(path.to_string());
                        }
                        if path != "-" && !path.is_empty() && explicit_output.is_none() {
                            explicit_output = Some(path.to_string());
                        }
                    }
                }
                i += 2;
                continue;
            }
        } else if let Some(val) = arg.strip_prefix("--emit=") {
            emit_types.extend(
                val.split(',')
                    .map(|s| s.split('=').next().unwrap_or(s).to_string()),
            );
            for part in val.split(',') {
                if let Some((kind, path)) = part.split_once('=') {
                    if kind == "link" && path != "-" {
                        explicit_link_output = Some(path.to_string());
                    }
                    if path != "-" && !path.is_empty() && explicit_output.is_none() {
                        explicit_output = Some(path.to_string());
                    }
                }
            }
            i += 1;
            continue;
        }

        // --out-dir <path> or --out-dir=<path>
        if arg == "--out-dir" {
            if let Some(next) = args.get(i + 1) {
                out_dir = Some(next.clone());
                i += 2;
                continue;
            }
        } else if let Some(val) = arg.strip_prefix("--out-dir=") {
            out_dir = Some(val.to_string());
            i += 1;
            continue;
        }

        // -o <path>
        if arg == "-o" {
            if let Some(next) = args.get(i + 1) {
                output_file = Some(next.clone());
                i += 2;
                continue;
            }
        }

        // -C <option> or -C<option> or --codegen <option>
        if arg == "-C" || arg == "--codegen" {
            if let Some(next) = args.get(i + 1) {
                if let Some(val) = next.strip_prefix("extra-filename=") {
                    extra_filename = Some(val.to_string());
                }
                i += 2;
                continue;
            }
        } else if let Some(rest) = arg.strip_prefix("-C") {
            if !rest.is_empty() {
                if let Some(val) = rest.strip_prefix("extra-filename=") {
                    extra_filename = Some(val.to_string());
                }
                i += 1;
                continue;
            }
        }

        // Known flags that take a value — skip both
        if let Some(&_flag) = RUSTC_FLAGS_WITH_VALUE.iter().find(|&&f| f == arg.as_str()) {
            i += 2;
            continue;
        }

        // Flags with = form (e.g., --edition=2021, --cfg=feature)
        if arg.starts_with("--") && arg.contains('=') {
            i += 1;
            continue;
        }

        // Any flag starting with -
        if arg.starts_with('-') {
            unknown_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // Positional arg — source file candidate (.rs)
        if arg.ends_with(".rs") {
            source_file = Some(arg.clone());
        }

        i += 1;
    }

    // No source file → non-cacheable (e.g., `rustc --version`)
    let source = match source_file {
        Some(s) => s,
        None => {
            return ParsedInvocation::NonCacheable {
                reason: "no .rs source file found".to_string(),
            };
        }
    };

    // Note: -C incremental is ignored for caching purposes (zccache#1021).
    // The incremental dir is excluded from the cache key, and we let rustc
    // use it on a miss. This is a DELIBERATE divergence from sccache,
    // which refuses to cache incremental compiles (its guidance is
    // CARGO_INCREMENTAL=0). zccache accepts the residual risk: incremental
    // can alter codegen-unit partitioning (and thus internal symbol
    // names) between otherwise-identical compiles, but the emitted
    // rlib/rmeta interface (SVH) is stable, and cargo passes incremental
    // on every dev-profile compile — refusing it would forfeit the bulk
    // of dev-loop caching.

    // Default crate type is bin if not specified
    if crate_types.is_empty() {
        crate_types.push("bin".to_string());
    }

    // Check all crate types are cacheable
    for ct in &crate_types {
        if !RUSTC_CACHEABLE_CRATE_TYPES.contains(&ct.as_str()) {
            return ParsedInvocation::NonCacheable {
                reason: format!("non-cacheable crate type: {ct}"),
            };
        }
    }

    // Determine primary output filename based on --emit and --crate-type.
    // - `--emit metadata` (no link) → rmeta sidecar
    // - `proc-macro` → host-side dylib (.so/.dylib/.dll, lib prefix on unix)
    // - `bin` → executable (no extension on unix, .exe on Windows)
    // - `staticlib` → static archive (.a)
    // - everything else cacheable → rlib
    let has_link_emit = emit_types.iter().any(|t| t == "link");
    let is_proc_macro = crate_types.iter().any(|t| t == "proc-macro");
    let is_bin = crate_types.iter().any(|t| t == "bin");
    let metadata_only = !has_link_emit && emit_types.iter().any(|t| t == "metadata");

    // Derive output path
    let primary_emit = if emit_types.iter().any(|kind| kind == "link") {
        Some("link")
    } else {
        emit_types
            .iter()
            .find(|kind| {
                matches!(
                    kind.as_str(),
                    "metadata"
                        | "dep-info"
                        | "obj"
                        | "asm"
                        | "llvm-ir"
                        | "llvm-bc"
                        | "bitcode"
                        | "mir"
                )
            })
            .map(String::as_str)
    };
    let output = if let Some(o) = output_file {
        o
    } else if let Some(o) = explicit_link_output {
        o
    } else if let Some(o) = explicit_output {
        o
    } else if let Some(ref dir) = out_dir {
        let name = crate_name.as_deref().unwrap_or("unknown");
        let suffix = extra_filename.as_deref().unwrap_or("");
        let filename = if primary_emit == Some("metadata") || metadata_only {
            format!("lib{name}{suffix}.rmeta")
        } else if primary_emit == Some("dep-info") {
            format!("{name}{suffix}.d")
        } else if primary_emit == Some("obj") {
            format!("{name}{suffix}.o")
        } else if primary_emit == Some("asm") {
            format!("{name}{suffix}.s")
        } else if primary_emit == Some("llvm-ir") {
            format!("{name}{suffix}.ll")
        } else if matches!(primary_emit, Some("llvm-bc" | "bitcode")) {
            format!("{name}{suffix}.bc")
        } else if primary_emit == Some("mir") {
            format!("{name}{suffix}.mir")
        } else if is_proc_macro {
            rustc_proc_macro_filename(name, suffix)
        } else if is_bin {
            rustc_bin_filename(name, suffix)
        } else if crate_types.iter().any(|t| t == "staticlib") {
            format!("lib{name}{suffix}.a")
        } else {
            format!("lib{name}{suffix}.rlib")
        };
        // Use NormalizedPath::join to handle platform path separators correctly
        NormalizedPath::new(dir)
            .join(filename)
            .to_string_lossy()
            .into_owned()
    } else {
        let name = crate_name.as_deref().unwrap_or_else(|| {
            std::path::Path::new(&source)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
        });
        let filename = if primary_emit == Some("metadata") || metadata_only {
            format!("lib{name}.rmeta")
        } else if primary_emit == Some("dep-info") {
            format!("{name}.d")
        } else if primary_emit == Some("obj") {
            format!("{name}.o")
        } else if primary_emit == Some("asm") {
            format!("{name}.s")
        } else if primary_emit == Some("llvm-ir") {
            format!("{name}.ll")
        } else if matches!(primary_emit, Some("llvm-bc" | "bitcode")) {
            format!("{name}.bc")
        } else if primary_emit == Some("mir") {
            format!("{name}.mir")
        } else if is_proc_macro {
            rustc_proc_macro_filename(name, "")
        } else if is_bin {
            rustc_bin_filename(name, "")
        } else if crate_types.iter().any(|t| t == "staticlib") {
            format!("lib{name}.a")
        } else {
            format!("lib{name}.rlib")
        };
        filename
    };

    ParsedInvocation::Cacheable(CacheableCompilation {
        compiler: NormalizedPath::new(compiler),
        family: CompilerFamily::Rustc,
        source_file: NormalizedPath::new(source),
        output_file: NormalizedPath::new(output),
        original_args: Arc::from(args.to_vec()),
        unknown_flags,
    })
}

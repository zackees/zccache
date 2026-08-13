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
///   `dylib` and general `cdylib` are deliberately NOT cacheable — dynamic
///   libraries embed platform linker state (soname/install-name, import
///   libs) that the artifact store does not model, so PyO3/maturin
///   `cdylib` final artifacts recompile every time while their rlib deps
///   still hit. A Dylint lint-library `cdylib` is the narrow exception:
///   its declared library and toolchain-qualified byte-copy sidecar are
///   modeled as one complete artifact set by the daemon.
const RUSTC_CACHEABLE_CRATE_TYPES: &[&str] = &["lib", "rlib", "staticlib", "proc-macro", "bin"];

/// Host dynamic-library file-name pattern for proc-macros, matching
/// rustc's output naming. Linux/macOS use the `lib` prefix; Windows
/// doesn't.
fn rustc_proc_macro_filename(crate_name: &str, extra: &str) -> String {
    if crate::platform::host::is_windows() {
        format!("{crate_name}{extra}.dll")
    } else if crate::platform::host::is_macos() {
        format!("lib{crate_name}{extra}.dylib")
    } else {
        format!("lib{crate_name}{extra}.so")
    }
}

/// Host dynamic-library file-name pattern for a Dylint lint cdylib.
fn rustc_dylint_cdylib_filename(crate_name: &str) -> String {
    if crate::platform::host::is_macos() {
        format!("lib{crate_name}.dylib")
    } else {
        format!("lib{crate_name}.so")
    }
}

fn is_dylint_linker(linker: Option<&str>) -> bool {
    linker.is_some_and(|linker| {
        std::path::Path::new(linker)
            .file_stem()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|stem| stem.eq_ignore_ascii_case("dylint-link"))
    })
}

fn is_dylint_library_out_dir(out_dir: Option<&str>) -> bool {
    let Some(out_dir) = out_dir else {
        return false;
    };
    let components: Vec<_> = std::path::Path::new(out_dir).components().collect();
    components.windows(2).any(|pair| {
        pair[0].as_os_str() == std::ffi::OsStr::new("dylint")
            && pair[1].as_os_str() == std::ffi::OsStr::new("libraries")
    })
}

/// Host executable file-name pattern for `--crate-type bin`. Windows
/// adds `.exe`; unix has no extension.
/// Primary-output extension for each non-link `--emit` kind.
///
/// The third field mirrors rustc's own asymmetry: only `metadata` carries the
/// `lib` prefix; every other emit kind names the file after the crate alone.
/// Keep this a table — it is a closed set defined by rustc, and expressing it
/// as a conditional chain is what let the same twelve arms be written twice.
const EMIT_OUTPUT_EXTENSIONS: &[(&str, &str, bool)] = &[
    ("metadata", "rmeta", true),
    ("dep-info", "d", false),
    ("obj", "o", false),
    ("asm", "s", false),
    ("llvm-ir", "ll", false),
    ("llvm-bc", "bc", false),
    ("bitcode", "bc", false),
    ("mir", "mir", false),
];

/// Everything that determines rustc's primary output filename.
///
/// Grouped into a struct so the two call sites — `--out-dir` present and
/// absent — differ only in how `name` and `suffix` are resolved, instead of
/// each restating the full dispatch.
struct RustcOutputShape<'a> {
    primary_emit: Option<&'a str>,
    metadata_only: bool,
    name: &'a str,
    /// `-C extra-filename`, or empty when the caller does not apply one.
    suffix: &'a str,
    target: Option<&'a str>,
    is_proc_macro: bool,
    is_bin: bool,
    is_dylint_cdylib: bool,
    is_staticlib: bool,
}

/// Resolve the filename rustc will write for this invocation.
///
/// Two dispatches in priority order: an explicit non-link `--emit` names the
/// file by emit kind, otherwise the crate type does. The platform-specific
/// helpers stay separate on purpose — the `.dll`/`.dylib`/`.so`/`.exe` split
/// and the `lib`-prefix asymmetry are OS facts, not cases to fold together.
fn rustc_primary_output_filename(shape: &RustcOutputShape<'_>) -> String {
    let RustcOutputShape {
        primary_emit,
        metadata_only,
        name,
        suffix,
        target,
        is_proc_macro,
        is_bin,
        is_dylint_cdylib,
        is_staticlib,
    } = *shape;

    if metadata_only {
        return format!("lib{name}{suffix}.rmeta");
    }
    if let Some(emit) = primary_emit {
        if let Some(&(_, extension, lib_prefixed)) = EMIT_OUTPUT_EXTENSIONS
            .iter()
            .find(|(kind, _, _)| *kind == emit)
        {
            return if lib_prefixed {
                format!("lib{name}{suffix}.{extension}")
            } else {
                format!("{name}{suffix}.{extension}")
            };
        }
    }
    if is_proc_macro {
        return rustc_proc_macro_filename(name, suffix);
    }
    if is_dylint_cdylib {
        return rustc_dylint_cdylib_filename(name);
    }
    if is_bin {
        return rustc_bin_filename(name, suffix, target);
    }
    if is_staticlib {
        return format!("lib{name}{suffix}.a");
    }
    format!("lib{name}{suffix}.rlib")
}

fn rustc_bin_filename(crate_name: &str, extra: &str, target: Option<&str>) -> String {
    let windows_target = target
        .map(|triple| triple.split('-').any(|part| part == "windows"))
        .unwrap_or_else(crate::platform::host::is_windows);
    if windows_target {
        format!("{crate_name}{extra}.exe")
    } else {
        format!("{crate_name}{extra}")
    }
}

fn is_randomized_autocfg_crate_name(crate_name: &str) -> bool {
    let Some((uuid, probe_id)) = crate_name
        .strip_prefix("autocfg_")
        .and_then(|suffix| suffix.split_once('_'))
    else {
        return false;
    };
    uuid.len() == 16
        && uuid.bytes().all(|byte| byte.is_ascii_hexdigit())
        && !probe_id.is_empty()
        && probe_id.bytes().all(|byte| byte.is_ascii_digit())
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
    let execution_args = args;
    let args = match super::dylint_inner_rustc_args(compiler, args) {
        Ok(Some((_inner_rustc, rustc_args))) => rustc_args,
        Ok(None) => args,
        Err(reason) => {
            return ParsedInvocation::NonCacheable {
                reason: reason.to_string(),
            };
        }
    };
    let mut crate_types: Vec<String> = Vec::new();
    let mut source_file: Option<String> = None;
    let mut output_file: Option<String> = None;
    let mut out_dir: Option<String> = None;
    let mut crate_name: Option<String> = None;
    let mut extra_filename: Option<String> = None;
    let mut linker: Option<String> = None;
    let mut target: Option<&str> = None;
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

        // --target <triple> or --target=<triple>
        if arg == "--target" {
            if let Some(next) = args.get(i + 1) {
                target = Some(next.as_str());
                i += 2;
                continue;
            }
        } else if let Some(val) = arg.strip_prefix("--target=") {
            target = Some(val);
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
                if let Some(val) = next.strip_prefix("linker=") {
                    linker = Some(val.to_string());
                }
                i += 2;
                continue;
            }
        } else if let Some(rest) = arg.strip_prefix("-C") {
            if !rest.is_empty() {
                if let Some(val) = rest.strip_prefix("extra-filename=") {
                    extra_filename = Some(val.to_string());
                }
                if let Some(val) = rest.strip_prefix("linker=") {
                    linker = Some(val.to_string());
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

    // autocfg 1.5+ deliberately puts a process-random UUID in each probe
    // crate name. rustc embeds that identity in the emitted LLVM IR, so the
    // invocation cannot hit across build-script processes even when a wrapper
    // has made the stdin source content-addressed.
    if crate_types == ["lib"]
        && emit_types.iter().any(|emit| emit == "llvm-ir")
        && crate_name
            .as_deref()
            .is_some_and(is_randomized_autocfg_crate_name)
    {
        return ParsedInvocation::NonCacheable {
            reason: "randomized autocfg probe crate name".to_string(),
        };
    }

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

    // The Dylint bootstrap is the only cdylib form whose full output set is
    // modeled. Keep it host-only and reject extra-filename because
    // dylint-link's package-name guard would not create the sidecar.
    let is_dylint_cdylib = !crate::platform::host::is_windows()
        && crate_types == ["cdylib"]
        && target.is_none()
        && extra_filename.as_deref().is_none_or(str::is_empty)
        && is_dylint_linker(linker.as_deref())
        && is_dylint_library_out_dir(out_dir.as_deref());

    // Check all crate types are cacheable.
    for ct in &crate_types {
        if !(RUSTC_CACHEABLE_CRATE_TYPES.contains(&ct.as_str())
            || ct == "cdylib" && is_dylint_cdylib)
        {
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
    } else {
        // The two remaining cases differ only in how the crate name and the
        // `-C extra-filename` suffix are resolved; the filename dispatch
        // itself is identical, so it lives in one place.
        //
        // Without `--out-dir` rustc writes into the cwd, an absent
        // `--crate-name` falls back to the source file stem, and no
        // `extra-filename` suffix applies.
        let (name, suffix) = if out_dir.is_some() {
            (
                crate_name.as_deref().unwrap_or("unknown"),
                extra_filename.as_deref().unwrap_or(""),
            )
        } else {
            let name = crate_name.as_deref().unwrap_or_else(|| {
                std::path::Path::new(&source)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
            });
            (name, "")
        };
        let filename = rustc_primary_output_filename(&RustcOutputShape {
            primary_emit,
            metadata_only,
            name,
            suffix,
            target,
            is_proc_macro,
            is_bin,
            is_dylint_cdylib,
            is_staticlib: crate_types.iter().any(|t| t == "staticlib"),
        });
        match out_dir {
            // NormalizedPath::join handles platform path separators correctly.
            Some(ref dir) => NormalizedPath::new(dir)
                .join(filename)
                .to_string_lossy()
                .into_owned(),
            None => filename,
        }
    };

    ParsedInvocation::Cacheable(CacheableCompilation {
        compiler: NormalizedPath::new(compiler),
        family: CompilerFamily::Rustc,
        source_file: NormalizedPath::new(source),
        output_file: NormalizedPath::new(output),
        original_args: Arc::from(execution_args.to_vec()),
        unknown_flags,
    })
}

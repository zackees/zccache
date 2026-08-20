//! Rustc argument parser for cache key computation.
//!
//! Extracts cache-relevant flags from rustc command lines. Separates
//! args that affect compilation output (included in cache key) from
//! args that are cosmetic or path-dependent (excluded).

use std::path::Path;

use zccache_core::NormalizedPath;

/// A parsed `--extern name=path` declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternCrate {
    /// The crate name (e.g., "serde").
    pub name: String,
    /// Path to the rlib/rmeta file.
    pub path: NormalizedPath,
}

/// Result of parsing rustc arguments for cache key computation.
#[derive(Debug, Clone)]
pub struct RustcParsedArgs {
    /// The source file (positional .rs arg).
    pub source_file: NormalizedPath,

    // â”€â”€ Cache-key fields (affect compilation output) â”€â”€
    /// `--crate-name` value.
    pub crate_name: Option<String>,
    /// `--crate-type` values (lib, rlib, staticlib).
    pub crate_types: Vec<String>,
    /// `--edition` value (2015, 2018, 2021, 2024).
    pub edition: Option<String>,
    /// `--emit` types (dep-info, metadata, link, etc.).
    pub emit_types: Vec<String>,
    /// Explicit `--emit=kind=path` destinations, resolved against `cwd`.
    pub explicit_emit_paths: Vec<(String, NormalizedPath)>,
    /// `--cfg` values (sorted for deterministic hashing).
    pub cfgs: Vec<String>,
    /// `--check-cfg` values (sorted).
    pub check_cfgs: Vec<String>,
    /// Cache-relevant `-C` codegen options in command-line order.
    /// Includes: opt-level, codegen-units, target-cpu, target-feature,
    /// lto, panic, debuginfo, strip, overflow-checks, embed-bitcode.
    /// Excludes: incremental and linker/pass-through options. Cargo metadata
    /// and extra-filename are tracked separately because they affect rustc
    /// artifact identity and output names.
    pub codegen_flags: Vec<String>,
    /// `--target` value (cross-compilation triple).
    pub target: Option<String>,
    /// `--cap-lints` value.
    pub cap_lints: Option<String>,
    /// `--extern` crate declarations (name + path for content hashing).
    pub externs: Vec<ExternCrate>,
    /// Lint flags: `-A`, `-W`, `-D`, `-F` (sorted).
    pub lint_flags: Vec<String>,
    /// Flags not recognized by the parser (sorted, hashed into key).
    pub unknown_flags: Vec<String>,

    // â”€â”€ Non-cache-key fields (needed for output path / depfile) â”€â”€
    /// `--out-dir` path.
    pub out_dir: Option<NormalizedPath>,
    /// `-C extra-filename=` value.
    pub extra_filename: Option<String>,
    /// `-C metadata=` value (cargo's disambiguation hash).
    pub cargo_metadata: Option<String>,
    /// `-C incremental=` path.
    pub incremental_dir: Option<NormalizedPath>,
    /// `-C linker=` path. Excluded from ordinary Rust keys, but required to
    /// model Dylint lint-library cdylibs and hash their linker identity.
    pub linker: Option<NormalizedPath>,
    /// `-C link-arg=` / `-C link-args=` values. These become cache-key
    /// material for the narrow Dylint cdylib lane.
    pub linker_args: Vec<String>,
    /// `--error-format` value.
    pub error_format: Option<String>,
    /// `--json` value.
    pub json_format: Option<String>,
    /// `--color` value.
    pub color: Option<String>,
    /// `--diagnostic-width` value.
    pub diagnostic_width: Option<String>,
    /// `-L` search paths.
    pub search_paths: Vec<NormalizedPath>,
    /// `--remap-path-prefix` values.
    pub remap_path_prefixes: Vec<String>,
    /// `--sysroot` path.
    pub sysroot: Option<NormalizedPath>,
    /// `-o` output file (explicit).
    pub output_file: Option<NormalizedPath>,
}

impl RustcParsedArgs {
    /// Return the effective value for a last-one-wins codegen option.
    #[must_use]
    pub fn effective_codegen_value(&self, key: &str) -> Option<&str> {
        self.codegen_flags.iter().rev().find_map(|flag| {
            let (candidate, value) = flag.split_once('=').unwrap_or((flag, ""));
            (candidate == key).then_some(value)
        })
    }
}

/// Codegen options excluded from cache key (cosmetic or path-dependent).
/// Any `-C` option NOT in this list is included in the cache key by default,
/// which is the safe choice: unknown options are assumed to affect output.
const EXCLUDED_CODEGEN: &[&str] = &["incremental", "save-temps", "remark"];

/// Parse rustc arguments into structured form for cache key computation.
///
/// `args` should be the arguments after the compiler executable.
/// Relative paths are resolved against `cwd`.
pub fn parse_rustc_args(args: &[String], cwd: &Path) -> RustcParsedArgs {
    let mut result = RustcParsedArgs {
        source_file: NormalizedPath::new(""),
        crate_name: None,
        crate_types: Vec::new(),
        edition: None,
        emit_types: Vec::new(),
        explicit_emit_paths: Vec::new(),
        cfgs: Vec::new(),
        check_cfgs: Vec::new(),
        codegen_flags: Vec::new(),
        target: None,
        cap_lints: None,
        externs: Vec::new(),
        lint_flags: Vec::new(),
        unknown_flags: Vec::new(),
        out_dir: None,
        extra_filename: None,
        cargo_metadata: None,
        incremental_dir: None,
        linker: None,
        linker_args: Vec::new(),
        error_format: None,
        json_format: None,
        color: None,
        diagnostic_width: None,
        search_paths: Vec::new(),
        remap_path_prefixes: Vec::new(),
        sysroot: None,
        output_file: None,
    };

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        // --edition <val> or --edition=<val>
        if let Some(val) = take_option(arg, "--edition", args.get(i + 1), &mut i) {
            result.edition = Some(val);
            continue;
        }

        // --crate-type <val> or --crate-type=<val>
        // Rustc accepts comma-separated types: --crate-type lib,rlib
        if let Some(val) = take_option(arg, "--crate-type", args.get(i + 1), &mut i) {
            result
                .crate_types
                .extend(val.split(',').map(|s| s.to_string()));
            continue;
        }

        // --crate-name <val> or --crate-name=<val>
        if let Some(val) = take_option(arg, "--crate-name", args.get(i + 1), &mut i) {
            result.crate_name = Some(val);
            continue;
        }

        // --emit <types> or --emit=<types>
        if let Some(val) = take_option(arg, "--emit", args.get(i + 1), &mut i) {
            for part in val.split(',') {
                // Handle --emit=dep-info=path/to/file form
                let emit_type = part.split('=').next().unwrap_or(part).to_string();
                if !result.emit_types.contains(&emit_type) {
                    result.emit_types.push(emit_type);
                }
                if let Some((kind, path)) = part.split_once('=') {
                    if path != "-" && !path.is_empty() {
                        result
                            .explicit_emit_paths
                            .push((kind.to_string(), resolve_path(path, cwd)));
                    }
                }
            }
            continue;
        }

        // --target <val> or --target=<val>
        if let Some(val) = take_option(arg, "--target", args.get(i + 1), &mut i) {
            result.target = Some(val);
            continue;
        }

        // --cap-lints <val>
        if let Some(val) = take_option(arg, "--cap-lints", args.get(i + 1), &mut i) {
            result.cap_lints = Some(val);
            continue;
        }

        // --cfg <val> or --cfg=<val>
        if let Some(val) = take_option(arg, "--cfg", args.get(i + 1), &mut i) {
            result.cfgs.push(val);
            continue;
        }

        // --check-cfg <val> or --check-cfg=<val>
        if let Some(val) = take_option(arg, "--check-cfg", args.get(i + 1), &mut i) {
            result.check_cfgs.push(val);
            continue;
        }

        // --extern <name=path> or --extern=<name=path>
        if let Some(val) = take_option(arg, "--extern", args.get(i + 1), &mut i) {
            if let Some((name, path)) = val.split_once('=') {
                // Handle noprelude:name=path form
                let actual_name = name.strip_prefix("noprelude:").unwrap_or(name);
                result.externs.push(ExternCrate {
                    name: actual_name.to_string(),
                    path: resolve_path(path, cwd),
                });
            }
            // --extern name (without =path) â€” no file to hash
            continue;
        }

        // --out-dir <path> or --out-dir=<path>
        if let Some(val) = take_option(arg, "--out-dir", args.get(i + 1), &mut i) {
            result.out_dir = Some(resolve_path(&val, cwd));
            continue;
        }

        // --error-format <val>
        if let Some(val) = take_option(arg, "--error-format", args.get(i + 1), &mut i) {
            result.error_format = Some(val);
            continue;
        }

        // --json <val>
        if let Some(val) = take_option(arg, "--json", args.get(i + 1), &mut i) {
            result.json_format = Some(val);
            continue;
        }

        // --color <val>
        if let Some(val) = take_option(arg, "--color", args.get(i + 1), &mut i) {
            result.color = Some(val);
            continue;
        }

        // --diagnostic-width <val>
        if let Some(val) = take_option(arg, "--diagnostic-width", args.get(i + 1), &mut i) {
            result.diagnostic_width = Some(val);
            continue;
        }

        // --sysroot <path>
        if let Some(val) = take_option(arg, "--sysroot", args.get(i + 1), &mut i) {
            result.sysroot = Some(resolve_path(&val, cwd));
            continue;
        }

        // --remap-path-prefix <val>
        if let Some(val) = take_option(arg, "--remap-path-prefix", args.get(i + 1), &mut i) {
            result.remap_path_prefixes.push(val);
            continue;
        }

        // --env-set <val> â€” skip (nightly feature, not cache-relevant)
        if let Some(_val) = take_option(arg, "--env-set", args.get(i + 1), &mut i) {
            continue;
        }

        // -o <path>
        if arg == "-o" {
            if let Some(next) = args.get(i + 1) {
                result.output_file = Some(resolve_path(next, cwd));
                i += 2;
                continue;
            }
        }

        // -L <path>
        if arg == "-L" {
            if let Some(next) = args.get(i + 1) {
                // -L [KIND=]PATH â€” strip the kind= prefix
                let path_str = next.split_once('=').map(|(_, p)| p).unwrap_or(next);
                result.search_paths.push(resolve_path(path_str, cwd));
                i += 2;
                continue;
            }
        } else if let Some(rest) = arg.strip_prefix("-L") {
            if !rest.is_empty() {
                let path_str = rest.split_once('=').map(|(_, p)| p).unwrap_or(rest);
                result.search_paths.push(resolve_path(path_str, cwd));
                i += 1;
                continue;
            }
        }

        // -l [KIND[:MODIFIERS]=]NAME — native link libraries, typically from
        // build-script `cargo:rustc-link-lib` directives (zackees/soldr#2313).
        // The NAME must key the unit: before this arm, `-l` fell into the
        // generic unknown-flag fall-through and its VALUE was dropped as a
        // stray positional, so a link unit whose build script changed its
        // native libs was served the previous run's cached artifact — a
        // cache hit masking a link error. The `--` guard keeps long flags
        // (`--library-path`-style spellings) out of this arm.
        if arg == "-l" {
            if let Some(next) = args.get(i + 1) {
                result.unknown_flags.push(format!("-l {next}"));
                i += 2;
                continue;
            }
        } else if !arg.starts_with("--") {
            if let Some(rest) = arg.strip_prefix("-l") {
                if !rest.is_empty() {
                    result.unknown_flags.push(format!("-l {rest}"));
                    i += 1;
                    continue;
                }
            }
        }

        // -C <option> or --codegen <option>
        if arg == "-C" || arg == "--codegen" {
            if let Some(next) = args.get(i + 1) {
                handle_codegen_option(next, cwd, &mut result);
                i += 2;
                continue;
            }
        } else if let Some(rest) = arg.strip_prefix("-C") {
            if !rest.is_empty() {
                handle_codegen_option(rest, cwd, &mut result);
                i += 1;
                continue;
            }
        }

        // Compiler shorthands participate in the same last-one-wins order as
        // their explicit -C forms. Normalize them in place so hashing and
        // output discovery see the effective value correctly.
        if arg == "-g" {
            handle_codegen_option("debuginfo=2", cwd, &mut result);
            i += 1;
            continue;
        }
        if arg == "-O" {
            handle_codegen_option("opt-level=2", cwd, &mut result);
            i += 1;
            continue;
        }

        // Lint flags: -A, -W, -D, -F
        if matches!(arg.as_str(), "-A" | "-W" | "-D" | "-F") {
            if let Some(next) = args.get(i + 1) {
                result.lint_flags.push(format!("{arg} {next}"));
                i += 2;
                continue;
            }
        }

        // -Z <option> â€” nightly flags. Consume both flag and value.
        if arg == "-Z" {
            if let Some(next) = args.get(i + 1) {
                result.unknown_flags.push(format!("-Z {next}"));
                i += 2;
                continue;
            }
        }

        // Any flag starting with -
        if arg.starts_with('-') {
            result.unknown_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // Positional arg â€” source file
        if arg.ends_with(".rs") {
            result.source_file = resolve_path(arg, cwd);
        }

        i += 1;
    }

    // Sort order-insensitive collections for deterministic hashing. Codegen
    // flags stay in command-line order because some options are additive and
    // others are last-one-wins; reordering them can create false cache hits.
    result.cfgs.sort();
    result.check_cfgs.sort();
    result.lint_flags.sort();
    result.unknown_flags.sort();

    result
}

/// Try to extract a `--flag value` or `--flag=value` option.
/// Returns the value and advances `i` appropriately.
fn take_option(arg: &str, flag: &str, next: Option<&String>, i: &mut usize) -> Option<String> {
    if arg == flag {
        if let Some(next_val) = next {
            *i += 2;
            return Some(next_val.clone());
        }
    } else if let Some(val) = arg.strip_prefix(&format!("{flag}=")) {
        *i += 1;
        return Some(val.to_string());
    }
    None
}

/// Process a `-C <option>` codegen flag.
fn handle_codegen_option(opt: &str, cwd: &Path, result: &mut RustcParsedArgs) {
    let (key, value) = opt.split_once('=').unwrap_or((opt, ""));

    // Excluded codegen options (not cache-relevant)
    if key == "metadata" {
        result.cargo_metadata = Some(value.to_string());
        return;
    }
    if key == "extra-filename" {
        result.extra_filename = Some(value.to_string());
        return;
    }
    if key == "incremental" {
        result.incremental_dir = Some(resolve_path(value, cwd));
        return;
    }
    if key == "linker" {
        result.linker = Some(resolve_path(value, cwd));
        return;
    }
    if matches!(key, "link-arg" | "link-args") {
        result.linker_args.push(opt.to_string());
        return;
    }
    if EXCLUDED_CODEGEN.contains(&key) {
        return;
    }

    result.codegen_flags.push(opt.to_string());
}

/// Resolve a path against cwd if relative.
fn resolve_path(path: &str, cwd: &Path) -> NormalizedPath {
    let p = Path::new(path);
    if p.is_absolute() {
        NormalizedPath::new(p)
    } else {
        NormalizedPath::new(cwd.join(p))
    }
}

#[cfg(test)]
#[path = "rustc_args/tests.rs"]
mod tests;

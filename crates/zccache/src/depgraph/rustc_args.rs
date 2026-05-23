//! Rustc argument parser for cache key computation.
//!
//! Extracts cache-relevant flags from rustc command lines. Separates
//! args that affect compilation output (included in cache key) from
//! args that are cosmetic or path-dependent (excluded).

use std::path::Path;

use zccache::core::NormalizedPath;

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
    /// `--cfg` values (sorted for deterministic hashing).
    pub cfgs: Vec<String>,
    /// `--check-cfg` values (sorted).
    pub check_cfgs: Vec<String>,
    /// Cache-relevant `-C` codegen options (sorted).
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

/// Codegen options excluded from cache key (cosmetic or path-dependent).
/// Any `-C` option NOT in this list is included in the cache key by default,
/// which is the safe choice: unknown options are assumed to affect output.
const EXCLUDED_CODEGEN: &[&str] = &[
    "incremental",
    "linker",
    "link-arg",
    "link-args",
    "save-temps",
    "remark",
];

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

    // Sort all collections for deterministic hashing
    result.cfgs.sort();
    result.check_cfgs.sort();
    result.codegen_flags.sort();
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
    if EXCLUDED_CODEGEN.contains(&key) {
        return;
    }

    // Cache-relevant codegen options
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
mod tests {
    use zccache::core::NormalizedPath;

    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    fn cwd() -> NormalizedPath {
        NormalizedPath::from("/project")
    }

    #[test]
    fn basic_parse_source_file() {
        let parsed = parse_rustc_args(&args(&["src/lib.rs"]), &cwd());
        assert_eq!(
            parsed.source_file,
            NormalizedPath::from("/project/src/lib.rs")
        );
    }

    #[test]
    fn parse_edition() {
        let parsed = parse_rustc_args(&args(&["--edition", "2021", "src/lib.rs"]), &cwd());
        assert_eq!(parsed.edition.as_deref(), Some("2021"));
    }

    #[test]
    fn parse_edition_equals_form() {
        let parsed = parse_rustc_args(&args(&["--edition=2021", "src/lib.rs"]), &cwd());
        assert_eq!(parsed.edition.as_deref(), Some("2021"));
    }

    #[test]
    fn parse_crate_type() {
        let parsed = parse_rustc_args(
            &args(&["--crate-type", "lib", "--crate-type", "rlib", "src/lib.rs"]),
            &cwd(),
        );
        assert_eq!(parsed.crate_types, vec!["lib", "rlib"]);
    }

    #[test]
    fn parse_crate_name() {
        let parsed = parse_rustc_args(&args(&["--crate-name", "mylib", "src/lib.rs"]), &cwd());
        assert_eq!(parsed.crate_name.as_deref(), Some("mylib"));
    }

    #[test]
    fn parse_emit_types() {
        let parsed = parse_rustc_args(
            &args(&["--emit=dep-info,metadata,link", "src/lib.rs"]),
            &cwd(),
        );
        assert_eq!(parsed.emit_types, vec!["dep-info", "metadata", "link"]);
    }

    #[test]
    fn parse_emit_with_paths() {
        // --emit=dep-info=/path/to/deps.d,metadata,link
        let parsed = parse_rustc_args(
            &args(&["--emit=dep-info=/tmp/deps.d,metadata,link", "src/lib.rs"]),
            &cwd(),
        );
        assert_eq!(parsed.emit_types, vec!["dep-info", "metadata", "link"]);
    }

    #[test]
    fn parse_cfg_values() {
        let parsed = parse_rustc_args(
            &args(&["--cfg", "feature=\"derive\"", "--cfg", "unix", "src/lib.rs"]),
            &cwd(),
        );
        // Sorted
        assert_eq!(parsed.cfgs, vec!["feature=\"derive\"", "unix"]);
    }

    #[test]
    fn parse_codegen_flags() {
        let parsed = parse_rustc_args(
            &args(&["-C", "opt-level=2", "-C", "debuginfo=2", "src/lib.rs"]),
            &cwd(),
        );
        // Sorted
        assert!(parsed.codegen_flags.contains(&"debuginfo=2".to_string()));
        assert!(parsed.codegen_flags.contains(&"opt-level=2".to_string()));
    }

    #[test]
    fn parse_codegen_concatenated() {
        let parsed = parse_rustc_args(&args(&["-Copt-level=3", "src/lib.rs"]), &cwd());
        assert!(parsed.codegen_flags.contains(&"opt-level=3".to_string()));
    }

    #[test]
    fn excluded_codegen_not_in_cache_key() {
        let parsed = parse_rustc_args(
            &args(&[
                "-C",
                "metadata=abc123",
                "-C",
                "extra-filename=-abc123",
                "-C",
                "incremental=/tmp/incr",
                "-C",
                "linker=cc",
                "src/lib.rs",
            ]),
            &cwd(),
        );
        // None of these should be in codegen_flags
        assert!(parsed.codegen_flags.is_empty());
        // But they should be in their dedicated fields
        assert_eq!(parsed.cargo_metadata.as_deref(), Some("abc123"));
        assert_eq!(parsed.extra_filename.as_deref(), Some("-abc123"));
        assert_eq!(
            parsed.incremental_dir,
            Some(NormalizedPath::from("/tmp/incr"))
        );
    }

    #[test]
    fn parse_extern_crates() {
        let parsed = parse_rustc_args(
            &args(&[
                "--extern",
                "serde=/target/deps/libserde.rlib",
                "--extern",
                "log=/target/deps/liblog.rmeta",
                "src/lib.rs",
            ]),
            &cwd(),
        );
        assert_eq!(parsed.externs.len(), 2);
        assert_eq!(parsed.externs[0].name, "serde");
        assert_eq!(
            parsed.externs[0].path,
            NormalizedPath::from("/target/deps/libserde.rlib")
        );
        assert_eq!(parsed.externs[1].name, "log");
    }

    #[test]
    fn parse_extern_noprelude() {
        let parsed = parse_rustc_args(
            &args(&[
                "--extern",
                "noprelude:core=/path/libcore.rlib",
                "src/lib.rs",
            ]),
            &cwd(),
        );
        assert_eq!(parsed.externs[0].name, "core");
    }

    #[test]
    fn search_paths_excluded_from_cache_key() {
        let parsed = parse_rustc_args(
            &args(&[
                "-L",
                "dependency=/target/deps",
                "-L",
                "native=/usr/lib",
                "src/lib.rs",
            ]),
            &cwd(),
        );
        assert_eq!(parsed.search_paths.len(), 2);
        // search_paths are stored but NOT in codegen_flags/cfgs/unknown_flags
        assert!(parsed.codegen_flags.is_empty());
        assert!(parsed.unknown_flags.is_empty());
    }

    #[test]
    fn out_dir_excluded_from_cache_key() {
        let parsed = parse_rustc_args(
            &args(&["--out-dir", "/target/debug/deps", "src/lib.rs"]),
            &cwd(),
        );
        assert_eq!(
            parsed.out_dir,
            Some(NormalizedPath::from("/target/debug/deps"))
        );
        assert!(parsed.unknown_flags.is_empty());
    }

    #[test]
    fn cosmetic_flags_excluded() {
        let parsed = parse_rustc_args(
            &args(&[
                "--error-format=json",
                "--json=diagnostic-rendered-ansi",
                "--color=always",
                "--diagnostic-width=80",
                "src/lib.rs",
            ]),
            &cwd(),
        );
        assert_eq!(parsed.error_format.as_deref(), Some("json"));
        assert_eq!(
            parsed.json_format.as_deref(),
            Some("diagnostic-rendered-ansi")
        );
        assert_eq!(parsed.color.as_deref(), Some("always"));
        assert_eq!(parsed.diagnostic_width.as_deref(), Some("80"));
        // None of these should be in unknown_flags
        assert!(parsed.unknown_flags.is_empty());
    }

    #[test]
    fn parse_target() {
        let parsed = parse_rustc_args(
            &args(&["--target", "x86_64-unknown-linux-gnu", "src/lib.rs"]),
            &cwd(),
        );
        assert_eq!(parsed.target.as_deref(), Some("x86_64-unknown-linux-gnu"));
    }

    #[test]
    fn parse_cap_lints() {
        let parsed = parse_rustc_args(&args(&["--cap-lints", "allow", "src/lib.rs"]), &cwd());
        assert_eq!(parsed.cap_lints.as_deref(), Some("allow"));
    }

    #[test]
    fn parse_lint_flags() {
        let parsed = parse_rustc_args(
            &args(&[
                "-A",
                "dead_code",
                "-W",
                "unused",
                "-D",
                "warnings",
                "src/lib.rs",
            ]),
            &cwd(),
        );
        assert_eq!(parsed.lint_flags.len(), 3);
        assert!(parsed.lint_flags.contains(&"-A dead_code".to_string()));
        assert!(parsed.lint_flags.contains(&"-D warnings".to_string()));
        assert!(parsed.lint_flags.contains(&"-W unused".to_string()));
    }

    #[test]
    fn parse_output_file() {
        let parsed = parse_rustc_args(&args(&["-o", "libfoo.rlib", "src/lib.rs"]), &cwd());
        assert_eq!(
            parsed.output_file,
            Some(NormalizedPath::from("/project/libfoo.rlib"))
        );
    }

    #[test]
    fn full_cargo_invocation() {
        let parsed = parse_rustc_args(
            &args(&[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "serde",
                "--emit=dep-info,metadata,link",
                "-C",
                "opt-level=2",
                "-C",
                "metadata=abc123def",
                "-C",
                "extra-filename=-abc123def",
                "--out-dir",
                "/target/release/deps",
                "-L",
                "dependency=/target/release/deps",
                "--extern",
                "serde_derive=/target/release/deps/libserde_derive-xyz.so",
                "--cap-lints",
                "allow",
                "--cfg",
                "feature=\"derive\"",
                "--cfg",
                "feature=\"std\"",
                "--error-format=json",
                "--json=diagnostic-rendered-ansi,artifacts,future-incompat",
                "--diagnostic-width=211",
                "-C",
                "linker=cc",
                "src/lib.rs",
            ]),
            &cwd(),
        );

        // Cache-key fields populated
        assert_eq!(parsed.edition.as_deref(), Some("2021"));
        assert_eq!(parsed.crate_types, vec!["lib"]);
        assert_eq!(parsed.crate_name.as_deref(), Some("serde"));
        assert_eq!(parsed.emit_types, vec!["dep-info", "metadata", "link"]);
        assert!(parsed.codegen_flags.contains(&"opt-level=2".to_string()));
        assert_eq!(parsed.cap_lints.as_deref(), Some("allow"));
        assert!(parsed.cfgs.contains(&"feature=\"derive\"".to_string()));
        assert!(parsed.cfgs.contains(&"feature=\"std\"".to_string()));
        assert_eq!(parsed.externs.len(), 1);
        assert_eq!(parsed.externs[0].name, "serde_derive");

        // Excluded fields populated but NOT in cache-key collections
        assert_eq!(parsed.cargo_metadata.as_deref(), Some("abc123def"));
        assert_eq!(parsed.extra_filename.as_deref(), Some("-abc123def"));
        assert_eq!(parsed.error_format.as_deref(), Some("json"));
        assert!(parsed.search_paths.len() == 1);
        assert!(parsed.unknown_flags.is_empty());
    }

    #[test]
    fn z_flag_with_value_captured() {
        let parsed = parse_rustc_args(
            &args(&["-Z", "macro-backtrace", "--crate-type", "lib", "src/lib.rs"]),
            &cwd(),
        );
        // -Z and its value should be combined into one entry
        assert!(
            parsed
                .unknown_flags
                .contains(&"-Z macro-backtrace".to_string()),
            "got: {:?}",
            parsed.unknown_flags
        );
    }

    #[test]
    fn z_flag_different_values_different_keys() {
        let parsed1 = parse_rustc_args(&args(&["-Z", "query-threads=4", "src/lib.rs"]), &cwd());
        let parsed2 = parse_rustc_args(&args(&["-Z", "query-threads=8", "src/lib.rs"]), &cwd());
        assert_ne!(parsed1.unknown_flags, parsed2.unknown_flags);
    }

    #[test]
    fn comma_separated_crate_types_split() {
        let parsed = parse_rustc_args(&args(&["--crate-type", "lib,rlib", "src/lib.rs"]), &cwd());
        assert_eq!(parsed.crate_types, vec!["lib", "rlib"]);
    }

    #[test]
    fn relative_paths_resolved_against_cwd() {
        let parsed = parse_rustc_args(&args(&["src/lib.rs"]), &cwd());
        assert_eq!(
            parsed.source_file,
            NormalizedPath::from("/project/src/lib.rs")
        );
    }

    #[test]
    fn absolute_paths_unchanged() {
        let parsed = parse_rustc_args(&args(&["/absolute/src/lib.rs"]), &cwd());
        assert_eq!(
            parsed.source_file,
            NormalizedPath::from("/absolute/src/lib.rs")
        );
    }

    #[test]
    fn check_cfg_parsed() {
        let parsed = parse_rustc_args(
            &args(&["--check-cfg", "cfg(feature, values(\"std\"))", "src/lib.rs"]),
            &cwd(),
        );
        assert_eq!(parsed.check_cfgs.len(), 1);
    }

    #[test]
    fn sysroot_parsed() {
        let parsed = parse_rustc_args(
            &args(&[
                "--sysroot",
                "/home/user/.rustup/toolchains/stable",
                "src/lib.rs",
            ]),
            &cwd(),
        );
        assert_eq!(
            parsed.sysroot,
            Some(NormalizedPath::from("/home/user/.rustup/toolchains/stable"))
        );
    }

    #[test]
    fn remap_path_prefix_parsed() {
        let parsed = parse_rustc_args(
            &args(&["--remap-path-prefix", "/home/user=/anon", "src/lib.rs"]),
            &cwd(),
        );
        assert_eq!(parsed.remap_path_prefixes, vec!["/home/user=/anon"]);
    }

    #[test]
    fn remap_path_prefix_equals_form_parsed() {
        let parsed = parse_rustc_args(
            &args(&["--remap-path-prefix=/home/user=/anon", "src/lib.rs"]),
            &cwd(),
        );
        assert_eq!(parsed.remap_path_prefixes, vec!["/home/user=/anon"]);
    }
}

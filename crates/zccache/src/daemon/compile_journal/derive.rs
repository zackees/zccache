//! Derivation helpers (pure functions, no daemon state).
//!
//! Per issue #256 part J: these parse rustc-style argument vectors into the
//! canonical strings the extended journal schema uses. They live in this
//! module so the writer can call them without crossing crate boundaries.
//! They are public so the eventual `--profile` plumbing (Wave 2) and any
//! analyzer-side tooling can reuse them.

/// Find `--crate-name <name>` or `--crate-name=<name>` in a rustc-style
/// argument vector. Returns `None` when the flag is missing or appears
/// at the end of the vector with no following value.
#[must_use]
pub fn derive_crate_name(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if let Some(rest) = a.strip_prefix("--crate-name=") {
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        } else if a == "--crate-name" {
            if let Some(next) = args.get(i + 1) {
                return Some(next.clone());
            }
            return None;
        }
        i += 1;
    }
    None
}

/// Find `--crate-type <type>` or `--crate-type=<type>` and normalize to
/// one of the seven canonical kinds the schema enumerates. Returns
/// `None` if no value is present or the value is unrecognized.
///
/// Special case: cargo invokes `build.rs` as
/// `--crate-name build_script_build --crate-type bin`. When we detect
/// the build-script crate-name, the kind is reported as
/// `"build-script"` regardless of the literal `--crate-type`.
#[must_use]
pub fn derive_crate_type(args: &[String]) -> Option<&'static str> {
    if derive_crate_name(args).as_deref() == Some("build_script_build") {
        return Some("build-script");
    }

    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let raw: Option<&str> = if let Some(rest) = a.strip_prefix("--crate-type=") {
            Some(rest)
        } else if a == "--crate-type" {
            args.get(i + 1).map(String::as_str)
        } else {
            None
        };
        if let Some(raw) = raw {
            // `--crate-type lib,rlib` is legal; take the first segment
            // since the journal field is scalar.
            let first = raw.split(',').next().unwrap_or(raw).trim();
            return match first {
                // Canonical seven (schema enum).
                "lib" => Some("lib"),
                "bin" => Some("bin"),
                "proc-macro" | "proc_macro" => Some("proc-macro"),
                "test" => Some("test"),
                "bench" => Some("bench"),
                "example" => Some("example"),
                _ => None,
            };
        }
        i += 1;
    }
    None
}

/// Map a canonical crate-type to the output-extension that rustc emits.
/// Returns `None` when the crate-type is missing or outside the
/// schema-recognized set.
#[must_use]
pub fn derive_output_ext(crate_type: Option<&str>) -> Option<&'static str> {
    match crate_type? {
        "lib" => Some("rlib"),
        "bin" | "build-script" | "test" | "bench" | "example" => Some("exe"),
        "proc-macro" => Some("so"),
        _ => None,
    }
}

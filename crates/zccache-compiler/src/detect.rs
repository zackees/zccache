//! Compiler family detection from the compiler executable path.

use super::CompilerFamily;

/// Source file extensions we recognize as C/C++.
pub(crate) const SOURCE_EXTENSIONS: &[&str] = &[
    "c", "cc", "cpp", "cxx", "c++", "C", "m", "mm", "i", "ii", "cppm", "ixx", "s", "S",
];

/// File extensions that imply module-interface mode even without `-x c++-module`.
pub(crate) const MODULE_EXTENSIONS: &[&str] = &["cppm", "ixx"];

fn executable_stem(executable: &str) -> &str {
    let basename = executable
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(executable);
    basename
        .strip_suffix(".exe")
        .or_else(|| basename.strip_suffix(".EXE"))
        .unwrap_or(basename)
}

/// Whether `executable` is Dylint's compiler driver.
///
/// Keep this deliberately exact. Treating arbitrary `*-driver` programs as
/// rustc-compatible would let an unrelated executable enter the Rust cache
/// path without a modeled argv or cache-key contract.
#[must_use]
pub fn is_dylint_driver(executable: &str) -> bool {
    executable_stem(executable) == "dylint-driver"
}

/// Return the inner rustc argv from Dylint's nested compiler shape.
///
/// Dylint invokes `<dylint-driver> <rustc> <rustc-args...>`. The outer
/// driver and complete argv must still be executed unchanged; this slice is
/// only for rustc-style parsing and key construction.
pub fn dylint_inner_rustc_args<'a>(
    compiler: &str,
    args: &'a [String],
) -> Result<Option<(&'a str, &'a [String])>, &'static str> {
    if !is_dylint_driver(compiler) {
        return Ok(None);
    }
    let Some((inner, rustc_args)) = args.split_first() else {
        return Err("dylint-driver requires an inner rustc executable");
    };
    let inner_stem = executable_stem(inner);
    if inner_stem != "rustc" && !inner_stem.starts_with("rustc-") {
        return Err("dylint-driver inner rustc executable is missing or unsupported");
    }
    Ok(Some((inner.as_str(), rustc_args)))
}

/// Detect the compiler family from the compiler path.
#[must_use]
pub fn detect_family(compiler: &str) -> CompilerFamily {
    // Split on both `/` and `\` so Windows-style paths work on all platforms.
    let basename = compiler.rsplit(['/', '\\']).next().unwrap_or(compiler);
    let name = match basename.rsplit_once('.') {
        Some((stem, _)) => stem,
        None => basename,
    };
    if name == "rustfmt" || name.starts_with("rustfmt-") {
        CompilerFamily::Rustfmt
    } else if is_dylint_driver(compiler)
        || name == "rustc"
        || name.starts_with("rustc-")
        || name == "clippy-driver"
        || name.starts_with("clippy-driver-")
    {
        CompilerFamily::Rustc
    } else if is_clang_cl_name(name) {
        // `clang-cl` speaks MSVC argument syntax. It must be classified as
        // Msvc so the MSVC parser handles `/Fo`, `/c`, etc. Misclassifying
        // it as Clang caused issue #261 (Windows builds with 0 cached / 0
        // cold / 0 non-cacheable despite total > 0).
        CompilerFamily::Msvc
    } else if name.contains("clang") || name == "emcc" || name == "em++" {
        CompilerFamily::Clang
    } else if name.eq_ignore_ascii_case("cl") {
        CompilerFamily::Msvc
    } else {
        CompilerFamily::Gcc
    }
}

/// Whether the executable basename refers to clang-cl (the MSVC-syntax driver).
///
/// Matches `clang-cl`, `clang-cl-17`, `Clang-CL.EXE`, etc. `name` is the
/// stem with any final `.<ext>` already stripped by `detect_family`.
pub(crate) fn is_clang_cl_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "clang-cl" || lower.starts_with("clang-cl-")
}

/// Check if a path looks like a C/C++ source file.
pub(crate) fn is_source_file(path: &str) -> bool {
    if let Some(ext) = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
    {
        SOURCE_EXTENSIONS.contains(&ext)
    } else {
        false
    }
}

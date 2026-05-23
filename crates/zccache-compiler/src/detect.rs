//! Compiler family detection from the compiler executable path.

use crate::CompilerFamily;

/// Source file extensions we recognize as C/C++.
pub(crate) const SOURCE_EXTENSIONS: &[&str] = &[
    "c", "cc", "cpp", "cxx", "c++", "C", "m", "mm", "i", "ii", "cppm", "ixx",
];

/// File extensions that imply module-interface mode even without `-x c++-module`.
pub(crate) const MODULE_EXTENSIONS: &[&str] = &["cppm", "ixx"];

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
    } else if name == "rustc"
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

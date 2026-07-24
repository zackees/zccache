//! Detect C/C++ time macros that make otherwise-identical compiles unstable.
//!
//! `__DATE__`, `__TIME__`, and `__TIMESTAMP__` are expanded by the
//! preprocessor at compile time.  Their spelling is part of the source hash,
//! but the expansion is not, so serving an artifact from a previous compile
//! would freeze its timestamp.  Keep those translation units out of every
//! compile cache layer unless/when an explicit sloppiness mode is introduced.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use zccache_core::NormalizedPath;

const TIME_MACROS: [&[u8]; 3] = [b"__DATE__", b"__TIME__", b"__TIMESTAMP__"];

/// The first time-macro use seen for each translation unit is logged loudly.
///
/// This is process-local on purpose: the warning tells an operator why a
/// particular daemon is bypassing the cache without turning a normal build
/// with several includes into a warning flood.
static WARNED_TRANSLATION_UNITS: OnceLock<Mutex<HashSet<NormalizedPath>>> = OnceLock::new();

/// A detected time macro and the input file that contains it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TimeMacroUse {
    pub(super) source_file: NormalizedPath,
    pub(super) input_file: NormalizedPath,
    pub(super) macro_name: &'static str,
}

/// Scan a C/C++ compilation's source and force-included text inputs.
///
/// This must run before the request-level cache lookup.  Checking after a
/// fast hit would still allow an existing timestamp-bearing artifact to be
/// replayed.  Files that are not UTF-8 text (notably binary PCH inputs) are
/// intentionally ignored: their tokens cannot be newly expanded during this
/// preprocessing pass.
pub(super) fn find_time_macro_use(
    compilation: &crate::compiler::CacheableCompilation,
    cwd: &Path,
) -> Option<TimeMacroUse> {
    if !matches!(
        compilation.family,
        crate::compiler::CompilerFamily::Gcc
            | crate::compiler::CompilerFamily::Clang
            | crate::compiler::CompilerFamily::Msvc
    ) {
        return None;
    }

    let source_file = absolute_path(&compilation.source_file, cwd);
    let mut inputs = vec![source_file.clone()];
    let parsed = if compilation.family == crate::compiler::CompilerFamily::Msvc
        || crate::compiler::parse_msvc::looks_like_msvc_args(&compilation.original_args)
    {
        crate::depgraph::msvc_args::parse_msvc_args(&compilation.original_args, cwd)
    } else {
        crate::depgraph::args::parse_gnu_args(&compilation.original_args, cwd)
    };
    inputs.extend(parsed.force_includes);

    let mut seen = HashSet::new();
    for input_file in inputs {
        if !seen.insert(input_file.clone()) {
            continue;
        }
        if let Some(macro_name) = find_time_macro_in_file(&input_file) {
            return Some(TimeMacroUse {
                source_file,
                input_file,
                macro_name,
            });
        }
    }
    None
}

/// Emit the once-per-translation-unit diagnostic for a cache bypass.
pub(super) fn warn_time_macro_uncacheable(found: &TimeMacroUse) {
    let warned = WARNED_TRANSLATION_UNITS.get_or_init(|| Mutex::new(HashSet::new()));
    let Ok(mut warned) = warned.lock() else {
        // Poisoning a diagnostic-only mutex must not change compilation
        // correctness.  The cache bypass already happened.
        return;
    };
    if !warned.insert(found.source_file.clone()) {
        return;
    }
    tracing::warn!(
        event = "time_macro_noncacheable",
        source = %found.source_file.display(),
        input = %found.input_file.display(),
        macro_name = found.macro_name,
        "bypassing compile cache because a C/C++ time macro has a nondeterministic expansion"
    );
    crate::core::lifecycle::write_event(
        crate::core::lifecycle::EVENT_TIME_MACRO_NONCACHEABLE,
        serde_json::json!({
            "source": found.source_file.display().to_string(),
            "input": found.input_file.display().to_string(),
            "macro_name": found.macro_name,
        }),
    );
}

fn absolute_path(path: &NormalizedPath, cwd: &Path) -> NormalizedPath {
    if path.is_absolute() {
        path.clone()
    } else {
        cwd.join(path).into()
    }
}

fn find_time_macro_in_file(path: &Path) -> Option<&'static str> {
    let bytes = std::fs::read(path).ok()?;
    let text = std::str::from_utf8(&bytes).ok()?;
    find_time_macro_in_text(text)
}

/// Return a macro that appears as a preprocessor token, not in a comment or
/// ordinary string/character literal.  This deliberately stays a small lexer
/// rather than trying to parse C++: macro expansion happens before C++ syntax
/// matters, and raw strings are handled as opaque literals.
fn find_time_macro_in_text(text: &str) -> Option<&'static str> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            b'"' => i = skip_quoted(bytes, i, b'"'),
            b'\'' => i = skip_quoted(bytes, i, b'\''),
            b'R' if bytes.get(i + 1) == Some(&b'"') => i = skip_raw_string(bytes, i),
            _ => {
                for macro_bytes in TIME_MACROS {
                    if is_identifier_at(bytes, i, macro_bytes) {
                        return match macro_bytes {
                            b"__DATE__" => Some("__DATE__"),
                            b"__TIME__" => Some("__TIME__"),
                            b"__TIMESTAMP__" => Some("__TIMESTAMP__"),
                            _ => None,
                        };
                    }
                }
                i += 1;
            }
        }
    }
    None
}

fn skip_quoted(bytes: &[u8], start: usize, quote: u8) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
        } else if bytes[i] == quote {
            return i + 1;
        } else {
            i += 1;
        }
    }
    bytes.len()
}

fn skip_raw_string(bytes: &[u8], start: usize) -> usize {
    let delimiter_start = start + 2;
    let Some(open_paren) = bytes[delimiter_start..]
        .iter()
        .position(|byte| *byte == b'(')
    else {
        return start + 1;
    };
    let open_paren = delimiter_start + open_paren;
    // C++ limits raw-string delimiters to 16 characters.  Anything longer is
    // not a raw literal, so resume normal token scanning at the R.
    if open_paren - delimiter_start > 16 {
        return start + 1;
    }
    let mut terminator = Vec::with_capacity(open_paren - delimiter_start + 2);
    terminator.push(b')');
    terminator.extend_from_slice(&bytes[delimiter_start..open_paren]);
    terminator.push(b'"');
    let content = &bytes[open_paren + 1..];
    content
        .windows(terminator.len())
        .position(|window| window == terminator.as_slice())
        .map_or(bytes.len(), |offset| {
            open_paren + 1 + offset + terminator.len()
        })
}

fn is_identifier_at(bytes: &[u8], start: usize, needle: &[u8]) -> bool {
    bytes.get(start..start + needle.len()) == Some(needle)
        && !bytes
            .get(start.wrapping_sub(1))
            .is_some_and(|byte| is_identifier_byte(*byte))
        && !bytes
            .get(start + needle.len())
            .is_some_and(|byte| is_identifier_byte(*byte))
}

fn is_identifier_byte(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_time_macros_as_tokens() {
        for macro_name in ["__DATE__", "__TIME__", "__TIMESTAMP__"] {
            assert_eq!(
                find_time_macro_in_text(&format!("int x = {macro_name};")),
                Some(macro_name)
            );
        }
    }

    #[test]
    fn ignores_non_expanding_text() {
        for text in [
            "// __DATE__\nint x;",
            "/* __TIME__ */ int x;",
            "const char *s = \"__TIMESTAMP__\";",
            "const char c = '__DATE__'[0];",
            "const char *s = R\"tag(__DATE__)tag\";",
            "int __DATE__suffix = 0;",
        ] {
            assert_eq!(find_time_macro_in_text(text), None, "{text}");
        }
    }
}

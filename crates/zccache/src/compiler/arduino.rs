//! Arduino `.ino` to `.ino.cpp` conversion via libclang.
//!
//! This mirrors the Arduino preprocessing step at a pragmatic level:
//! extract forward declarations from top-level function definitions in the
//! main sketch, prepend them to a generated C++ translation unit, and keep
//! diagnostics/source mapping stable with `#line` directives.

use std::cell::RefCell;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use clang::{Clang, Entity, EntityKind, Index};
use thiserror::Error;
use zccache::core::NormalizedPath;

/// Conversion settings for `.ino` parsing and `.ino.cpp` generation.
#[derive(Debug, Clone)]
pub struct ArduinoConversionOptions {
    /// Extra compiler arguments passed through to libclang.
    pub clang_args: Vec<String>,
    /// Whether to inject `#include <Arduino.h>` into the generated `.cpp`.
    pub inject_arduino_include: bool,
}

impl Default for ArduinoConversionOptions {
    fn default() -> Self {
        Self {
            clang_args: Vec::new(),
            inject_arduino_include: true,
        }
    }
}

/// A synthesized function prototype plus the source line it originated from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedPrototype {
    pub declaration: String,
    pub line: u32,
}

/// Full `.ino.cpp` generation output.
#[derive(Debug, Clone)]
pub struct GeneratedInoCpp {
    pub cpp: String,
    pub prototypes: Vec<ExtractedPrototype>,
    pub diagnostics: Vec<String>,
}

/// Errors returned by the Arduino conversion pipeline.
#[derive(Debug, Error)]
pub enum ArduinoError {
    #[error("unable to locate libclang; set LIBCLANG_PATH or install LLVM/Clang")]
    MissingLibClang,
    #[error("source file is not valid UTF-8: {0}")]
    NonUtf8Path(NormalizedPath),
    #[error("failed to read {path}: {source}")]
    ReadFile {
        path: NormalizedPath,
        #[source]
        source: std::io::Error,
    },
    #[error("libclang failed to parse {path}: {message}")]
    Parse {
        path: NormalizedPath,
        message: String,
    },
    #[error("failed to extract prototype for function on line {line}")]
    PrototypeExtraction { line: u32 },
}

/// Return the discovered `libclang` binary path, if available.
#[must_use]
pub fn libclang_path() -> Option<NormalizedPath> {
    discover_libclang_path()
}

/// Return whether the current thread can load and use libclang.
#[must_use]
pub fn can_load_libclang() -> bool {
    ensure_libclang_env().is_ok() && current_thread_clang().is_ok()
}

/// Return the discovered `libclang` binary path and hash it for cache keys.
pub fn libclang_hash() -> Option<zccache::hash::ContentHash> {
    libclang_path()
        .as_ref()
        .and_then(|path| zccache::hash::hash_file(path).ok())
}

/// Generate an Arduino-style `.ino.cpp` file from an `.ino` source.
pub fn generate_ino_cpp(
    input: &Path,
    options: &ArduinoConversionOptions,
) -> Result<GeneratedInoCpp, ArduinoError> {
    static LIBCLANG_GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = LIBCLANG_GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("libclang parse mutex poisoned");

    ensure_libclang_env()?;

    let normalized_input = NormalizedPath::from(input);
    let source = fs::read_to_string(input).map_err(|source| ArduinoError::ReadFile {
        path: normalized_input.clone(),
        source,
    })?;
    let input_str = input
        .to_str()
        .ok_or_else(|| ArduinoError::NonUtf8Path(normalized_input.clone()))?;

    THREAD_LOCAL_CLANG.with(|_| {
        let clang = current_thread_clang()?;

        let index = Index::new(clang, false, false);
        let mut parser = index.parser(input_str);

        let mut args = vec![
            "-x".to_string(),
            "c++".to_string(),
            "-std=gnu++17".to_string(),
        ];
        args.extend(options.clang_args.clone());
        parser.arguments(&args);

        let tu = parser.parse().map_err(|err| ArduinoError::Parse {
            path: normalized_input.clone(),
            message: err.to_string(),
        })?;

        let diagnostics = tu
            .get_diagnostics()
            .into_iter()
            .map(|d| d.get_text())
            .collect::<Vec<_>>();

        let root = tu.get_entity();
        let root_children = root.get_children();
        let declared = collect_existing_declarations(&root_children);

        let mut prototypes = Vec::new();
        for entity in root_children {
            if !is_top_level_function_definition(&entity) {
                continue;
            }

            let usr = entity.get_usr().map(|usr| usr.0);
            if usr.as_ref().is_some_and(|usr| declared.contains(usr)) {
                continue;
            }

            let line = entity
                .get_location()
                .map(|loc| loc.get_spelling_location().line)
                .unwrap_or(1);
            let declaration = extract_function_prototype(&source, &entity)
                .ok_or(ArduinoError::PrototypeExtraction { line })?;
            prototypes.push(ExtractedPrototype { declaration, line });
        }

        prototypes.sort_by_key(|p| p.line);

        let cpp = build_generated_cpp(input, &source, &prototypes, options.inject_arduino_include);
        Ok(GeneratedInoCpp {
            cpp,
            prototypes,
            diagnostics,
        })
    })
}

thread_local! {
    static THREAD_LOCAL_CLANG: RefCell<Option<&'static Clang>> = const { RefCell::new(None) };
}

fn current_thread_clang() -> Result<&'static Clang, ArduinoError> {
    THREAD_LOCAL_CLANG.with(|slot| {
        let mut slot = slot.borrow_mut();
        match *slot {
            Some(clang) => Ok(clang),
            None => {
                let leaked: &'static Clang = Box::leak(Box::new(
                    Clang::new().map_err(|_| ArduinoError::MissingLibClang)?,
                ));
                *slot = Some(leaked);
                Ok(leaked)
            }
        }
    })
}

fn discover_libclang_path() -> Option<NormalizedPath> {
    static DISCOVERED: OnceLock<Option<NormalizedPath>> = OnceLock::new();
    DISCOVERED
        .get_or_init(|| {
            if let Ok(path) = std::env::var("LIBCLANG_PATH") {
                let path = NormalizedPath::from(path);
                if path.is_file() {
                    return Some(path);
                }
                let candidate = path.join(libclang_filename());
                if candidate.exists() {
                    return Some(candidate);
                }
            }

            default_libclang_candidates()
                .into_iter()
                .find(|candidate| candidate.exists())
        })
        .clone()
}

fn ensure_libclang_env() -> Result<(), ArduinoError> {
    let path = libclang_path().ok_or(ArduinoError::MissingLibClang)?;
    if let Some(parent) = path.parent() {
        if std::env::var_os("LIBCLANG_PATH").is_none() {
            std::env::set_var("LIBCLANG_PATH", parent);
        }
        let current_path = std::env::var_os("PATH").unwrap_or_default();
        let parent_os = parent.as_os_str();
        let needs_path = !std::env::split_paths(&current_path).any(|entry| entry == parent);
        if needs_path {
            let joined = std::env::join_paths(
                std::iter::once(parent_os.to_owned())
                    .chain(std::env::split_paths(&current_path).map(|p| p.into_os_string())),
            )
            .map_err(|_| ArduinoError::MissingLibClang)?;
            std::env::set_var("PATH", joined);
        }
    }
    Ok(())
}

fn default_libclang_candidates() -> Vec<NormalizedPath> {
    #[cfg(windows)]
    {
        vec![
            NormalizedPath::from(r"C:\Program Files\LLVM\bin\libclang.dll"),
            NormalizedPath::from(r"C:\Program Files\LLVM\lib\libclang.dll"),
            NormalizedPath::from(r"C:\Program Files\doxygen\bin\libclang.dll"),
        ]
    }
    #[cfg(target_os = "macos")]
    {
        vec![
            NormalizedPath::from("/opt/homebrew/opt/llvm/lib/libclang.dylib"),
            NormalizedPath::from("/usr/local/opt/llvm/lib/libclang.dylib"),
            NormalizedPath::from("/Library/Developer/CommandLineTools/usr/lib/libclang.dylib"),
        ]
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        vec![
            NormalizedPath::from("/usr/lib/llvm-18/lib/libclang.so"),
            NormalizedPath::from("/usr/lib/llvm-17/lib/libclang.so"),
            NormalizedPath::from("/usr/lib/llvm-16/lib/libclang.so"),
            NormalizedPath::from("/usr/lib/libclang.so"),
            NormalizedPath::from("/usr/local/lib/libclang.so"),
        ]
    }
}

#[cfg(windows)]
fn libclang_filename() -> &'static str {
    "libclang.dll"
}

#[cfg(target_os = "macos")]
fn libclang_filename() -> &'static str {
    "libclang.dylib"
}

#[cfg(all(unix, not(target_os = "macos")))]
fn libclang_filename() -> &'static str {
    "libclang.so"
}

fn collect_existing_declarations(entities: &[Entity<'_>]) -> HashSet<String> {
    entities
        .iter()
        .filter(|entity| {
            entity.get_kind() == EntityKind::FunctionDecl
                && !entity.is_definition()
                && entity
                    .get_location()
                    .is_some_and(|location| location.is_in_main_file())
        })
        .filter_map(|entity| entity.get_usr().map(|usr| usr.0))
        .collect()
}

fn is_top_level_function_definition(entity: &Entity<'_>) -> bool {
    entity.get_kind() == EntityKind::FunctionDecl
        && entity.is_definition()
        && entity
            .get_location()
            .is_some_and(|location| location.is_in_main_file())
}

fn extract_function_prototype(source: &str, entity: &Entity<'_>) -> Option<String> {
    let range = entity.get_range()?;
    let start = range.get_start().get_spelling_location().offset as usize;
    let body_start = function_body_start(entity)? as usize;

    if body_start <= start || body_start > source.len() {
        return None;
    }

    let prefix = source.get(start..body_start)?.trim_end();
    let signature = strip_default_arguments(prefix).trim().to_string();
    if signature.is_empty() {
        return None;
    }
    Some(format!("{};", signature.trim_end_matches(';').trim_end()))
}

fn function_body_start(entity: &Entity<'_>) -> Option<u32> {
    entity
        .get_children()
        .into_iter()
        .find(|child| child.get_kind() == EntityKind::CompoundStmt)
        .and_then(|child| child.get_range())
        .map(|range| range.get_start().get_spelling_location().offset)
}

fn strip_default_arguments(signature: &str) -> String {
    let mut out = String::with_capacity(signature.len());
    let chars: Vec<char> = signature.chars().collect();

    let mut i = 0usize;
    let mut param_depth = 0usize;
    let mut skipping_default = false;
    let mut nested_round = 0usize;
    let mut nested_square = 0usize;
    let mut nested_brace = 0usize;
    let mut nested_angle = 0usize;
    let mut in_string: Option<char> = None;
    let mut escaped = false;

    while i < chars.len() {
        let ch = chars[i];

        if let Some(quote) = in_string {
            if !skipping_default {
                out.push(ch);
            }
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                in_string = None;
            }
            i += 1;
            continue;
        }

        if ch == '"' || ch == '\'' {
            if !skipping_default {
                out.push(ch);
            }
            in_string = Some(ch);
            i += 1;
            continue;
        }

        if !skipping_default {
            match ch {
                '(' => {
                    param_depth += 1;
                    out.push(ch);
                }
                ')' => {
                    param_depth = param_depth.saturating_sub(1);
                    out.push(ch);
                }
                '=' if param_depth == 1 => {
                    while out.ends_with(char::is_whitespace) {
                        out.pop();
                    }
                    skipping_default = true;
                }
                _ => out.push(ch),
            }
            i += 1;
            continue;
        }

        match ch {
            '(' => nested_round += 1,
            ')' => {
                if nested_round > 0 {
                    nested_round -= 1;
                } else if nested_square == 0 && nested_brace == 0 && nested_angle == 0 {
                    out.push(')');
                    param_depth = param_depth.saturating_sub(1);
                    skipping_default = false;
                }
            }
            '[' => nested_square += 1,
            ']' => nested_square = nested_square.saturating_sub(1),
            '{' => nested_brace += 1,
            '}' => nested_brace = nested_brace.saturating_sub(1),
            '<' => nested_angle += 1,
            '>' => nested_angle = nested_angle.saturating_sub(1),
            ',' if nested_round == 0
                && nested_square == 0
                && nested_brace == 0
                && nested_angle == 0 =>
            {
                out.push(',');
                skipping_default = false;
            }
            _ => {}
        }
        i += 1;
    }

    out
}

fn build_generated_cpp(
    input: &Path,
    source: &str,
    prototypes: &[ExtractedPrototype],
    inject_arduino_include: bool,
) -> String {
    let mut out = String::new();

    if inject_arduino_include {
        out.push_str("#include <Arduino.h>\n");
    }
    if !prototypes.is_empty() {
        out.push('\n');
        for prototype in prototypes {
            out.push_str(&format!(
                "#line {} \"{}\"\n{}\n",
                prototype.line,
                input.display(),
                prototype.declaration
            ));
        }
    }

    out.push_str(&format!("\n#line 1 \"{}\"\n", input.display()));
    out.push_str(source);
    if !source.ends_with('\n') {
        out.push('\n');
    }
    out
}

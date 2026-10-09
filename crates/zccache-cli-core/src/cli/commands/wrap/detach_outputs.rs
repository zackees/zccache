//! #1791: copy-detach read-only rustc outputs before a `ZCCACHE_DISABLE=1`
//! passthrough.
//!
//! A cache hit under a hardlinking mode can leave an output at a path rustc
//! is about to write that rustc's `check_file_is_writeable` would refuse:
//! `Permissions::readonly()` must be false before rustc renames over the
//! output. Sealing keeps it false — `r--rw-r--` on Unix, a deny ACE on
//! Windows since kernal-api 0.1.29 (#1791) — but a blob sealed by an older
//! build can still carry the Windows `READONLY` attribute. The bypassed
//! wrapper therefore replaces each read-only output the invocation will
//! write with a private writable copy first, as defense in depth; the
//! shared cache blob is never written.
//!
//! Only the `ZCCACHE_DISABLE` passthrough calls this. It adds nothing to the
//! hit path (no daemon, no IPC) and touches no file that is not read-only.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The output locations one rustc invocation names on its command line.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct RustcOutputs {
    pub(super) out_dir: Option<PathBuf>,
    pub(super) output_file: Option<PathBuf>,
    pub(super) crate_name: Option<String>,
    pub(super) extra_filename: Option<String>,
}

pub(super) fn parse(args: &[String]) -> RustcOutputs {
    let mut parsed = RustcOutputs::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let mut value_of = |flag: &str| -> Option<String> {
            if arg == flag {
                iter.next().cloned()
            } else {
                arg.strip_prefix(flag)
                    .and_then(|rest| rest.strip_prefix('='))
                    .map(str::to_owned)
            }
        };
        if let Some(value) = value_of("--out-dir") {
            parsed.out_dir = Some(PathBuf::from(value));
        } else if let Some(value) = value_of("--crate-name") {
            parsed.crate_name = Some(value);
        } else if arg == "-o" {
            parsed.output_file = iter.next().map(PathBuf::from);
        } else if let Some(rest) = arg.strip_prefix("-o").filter(|rest| !rest.starts_with('-')) {
            parsed.output_file = Some(PathBuf::from(rest));
        } else if arg == "-C" || arg.starts_with("-C") {
            let codegen = if arg == "-C" {
                iter.next().map(String::as_str).unwrap_or("")
            } else {
                &arg[2..]
            };
            if let Some(extra) = codegen.strip_prefix("extra-filename=") {
                parsed.extra_filename = Some(extra.to_owned());
            }
        }
    }
    parsed
}

/// Whether `name` is an output of crate `stem` (`lib<stem>.rlib`,
/// `<stem>.exe`, `lib<stem>.rmeta`, `<stem>.d`, ...).
fn is_crate_output(name: &str, stem: &str) -> bool {
    let base = name.split('.').next().unwrap_or(name);
    base == stem || base.strip_prefix("lib") == Some(stem)
}

fn is_readonly_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().readonly())
        .unwrap_or(false)
}

/// Replace the read-only `path` by an independent writable copy of it.
fn detach(path: &Path) -> std::io::Result<()> {
    let mut temp_name = OsString::from(".zccache-detach-");
    temp_name.push(path.file_name().unwrap_or_default());
    let temp = path.with_file_name(temp_name);
    // Copy contents only: `std::fs::copy` would carry the READONLY attribute
    // over on Windows, defeating the point.
    let copied = std::fs::File::open(path).and_then(|mut source| {
        let mut target = std::fs::File::create(&temp)?;
        std::io::copy(&mut source, &mut target).map(drop)
    });
    if let Err(error) = copied {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    // Unix unlinks regardless of the file's mode; Windows refuses to delete a
    // READONLY file, and the attribute is shared with the cache blob. Clear it
    // only in that case. The daemon re-seals the blob on its next hit, and
    // digest verification refuses a modified blob before serving it.
    let removed = std::fs::remove_file(path).or_else(|error| {
        if error.kind() != std::io::ErrorKind::PermissionDenied {
            return Err(error);
        }
        crate::platform::fs::permissions::set_readonly(path, false)?;
        std::fs::remove_file(path)
    });
    match removed.and_then(|()| std::fs::rename(&temp, path)) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            Err(error)
        }
    }
}

/// Detach every read-only output the rustc invocation `tool_args` will write.
/// Best effort: a failure leaves rustc to report its own error.
pub(super) fn detach_readonly_rustc_outputs(tool_args: &[String]) {
    let parsed = parse(tool_args);
    let mut candidates = Vec::new();
    if let Some(file) = parsed.output_file {
        candidates.push(file);
    }
    if let Some(dir) = parsed.out_dir {
        let stem = parsed.crate_name.map(|name| {
            format!(
                "{}{}",
                name.replace('-', "_"),
                parsed.extra_filename.unwrap_or_default()
            )
        });
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let matches = match (&stem, name.to_str()) {
                    (Some(stem), Some(name)) => is_crate_output(name, stem),
                    (Some(_), None) => false,
                    (None, _) => true,
                };
                if matches {
                    candidates.push(entry.path());
                }
            }
        }
    }
    for path in candidates.into_iter().filter(|path| is_readonly_file(path)) {
        if let Err(error) = detach(&path) {
            eprintln!(
                "zccache[warn][F]: could not detach read-only output {}: {error}",
                path.display()
            );
        }
    }
}

/// Whether `tool` is rustc (`rustc`, `rustc.exe`, or a path to either).
pub(super) fn is_rustc(tool: &Path) -> bool {
    tool.file_stem().and_then(|stem| stem.to_str()) == Some("rustc")
}

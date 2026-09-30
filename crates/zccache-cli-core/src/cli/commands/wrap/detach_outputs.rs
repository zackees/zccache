//! #1791: copy-detach read-only rustc outputs before a `ZCCACHE_DISABLE=1`
//! passthrough.
//!
//! A cache hit under a hardlinking mode can leave a read-only output at a path
//! rustc is about to write. rustc refuses to replace an output whose
//! `Permissions::readonly()` is true (`check_file_is_writeable`). On Unix
//! zccache seals blobs `r--rw-r--` so that is never true, but Windows has no
//! such mode and the seal is the `READONLY` attribute. The bypassed wrapper
//! therefore replaces each read-only output the invocation will write with a
//! private writable copy first; the shared cache blob is never written.
//!
//! Only the `ZCCACHE_DISABLE` passthrough calls this. It adds nothing to the
//! hit path (no daemon, no IPC) and touches no file that is not read-only.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The output locations one rustc invocation names on its command line.
#[derive(Debug, Default, PartialEq, Eq)]
struct RustcOutputs {
    out_dir: Option<PathBuf>,
    output_file: Option<PathBuf>,
    crate_name: Option<String>,
    extra_filename: Option<String>,
}

fn parse(args: &[String]) -> RustcOutputs {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    /// A read-only hardlink of `blob` at `output`, as a LINK-mode hit leaves.
    fn readonly_link(blob: &Path, output: &Path) {
        std::fs::write(blob, b"cached").unwrap();
        std::fs::hard_link(blob, output).unwrap();
        crate::platform::fs::permissions::set_readonly(output, true).unwrap();
        assert!(std::fs::metadata(output).unwrap().permissions().readonly());
    }

    #[test]
    fn parses_cargo_style_invocation() {
        let parsed = parse(&args(&[
            "--crate-name",
            "probe",
            "--out-dir=/t/deps",
            "-C",
            "extra-filename=-abc",
            "-o",
            "x.o",
        ]));
        assert_eq!(parsed.out_dir, Some(PathBuf::from("/t/deps")));
        assert_eq!(parsed.crate_name.as_deref(), Some("probe"));
        assert_eq!(parsed.extra_filename.as_deref(), Some("-abc"));
        assert_eq!(parsed.output_file, Some(PathBuf::from("x.o")));
        let glued = parse(&args(&["-Cextra-filename=-z", "--out-dir", "d"]));
        assert_eq!(glued.extra_filename.as_deref(), Some("-z"));
        assert_eq!(glued.out_dir, Some(PathBuf::from("d")));
    }

    #[test]
    fn detaches_only_this_crates_readonly_outputs_and_spares_the_blob() {
        let dir = tempfile::tempdir().unwrap();
        let deps = dir.path().join("deps");
        std::fs::create_dir(&deps).unwrap();
        let blob = dir.path().join("blob");
        let mine = deps.join("libprobe-abc.rmeta");
        readonly_link(&blob, &mine);
        // Another crate's read-only output and this crate's writable one.
        let other_blob = dir.path().join("other-blob");
        let other = deps.join("libother-abc.rmeta");
        readonly_link(&other_blob, &other);

        detach_readonly_rustc_outputs(&args(&[
            "--crate-name",
            "probe",
            "-C",
            "extra-filename=-abc",
            "--out-dir",
            deps.to_str().unwrap(),
        ]));

        assert!(
            !std::fs::metadata(&mine).unwrap().permissions().readonly(),
            "the invocation's output must be writable so rustc replaces it"
        );
        assert!(
            crate::platform::fs::identity::file_identity(&mine).unwrap()
                != crate::platform::fs::identity::file_identity(&blob).unwrap(),
            "the output must be an independent copy, not a link to the blob"
        );
        assert_eq!(std::fs::read(&mine).unwrap(), b"cached");
        assert_eq!(std::fs::read(&blob).unwrap(), b"cached");
        assert!(
            std::fs::metadata(&other).unwrap().permissions().readonly(),
            "another crate's output is not this invocation's to touch"
        );
        assert!(!deps.join(".zccache-detach-libprobe-abc.rmeta").exists());
    }

    #[test]
    fn detaches_explicit_output_file() {
        let dir = tempfile::tempdir().unwrap();
        let blob = dir.path().join("blob");
        let out = dir.path().join("prog");
        readonly_link(&blob, &out);
        detach_readonly_rustc_outputs(&args(&["-o", out.to_str().unwrap()]));
        assert!(!std::fs::metadata(&out).unwrap().permissions().readonly());
        assert_eq!(std::fs::read(&out).unwrap(), b"cached");
    }

    #[test]
    fn recognises_rustc_tool_names() {
        assert!(is_rustc(Path::new("/x/bin/rustc")));
        assert!(is_rustc(Path::new("rustc.exe")));
        assert!(!is_rustc(Path::new("clang")));
    }
}

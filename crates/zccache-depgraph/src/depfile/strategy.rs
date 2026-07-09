//! Depfile strategy selection: decide how to obtain `.d` output for a
//! compile (inject `-MD -MF`, defer to a user-provided path, parse
//! MSVC `/showIncludes`, or treat the compiler as unsupported).

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use super::super::args::UserDepFlags;
use zccache_core::NormalizedPath;

/// How to obtain the depfile for a compilation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepfileStrategy {
    /// We injected `-MD -MF <path>` — read and clean up after compilation.
    Injected { path: NormalizedPath },
    /// User already had `-MF <path>` — read it (don't delete).
    UserSpecified { path: NormalizedPath },
    /// User had `-MD` but no `-MF` — derive path from output stem.
    UserDefault { path: NormalizedPath },
    /// MSVC `/showIncludes` — parse stderr after compilation.
    ShowIncludes,
    /// Compiler doesn't support depfiles — use fallback scanner.
    Unsupported,
}

/// Where the user wants the depfile written on disk for the current
/// compile, derived from the user's existing `-MD`/`-MF` flags.
///
/// Returns `Some(path)` only when the user explicitly asked for a depfile
/// — either via `-MF <path>` (`UserSpecified` strategy) or via `-MD` with
/// the implicit `<output>.d` (`UserDefault` strategy). Returns `None` for
/// the `Injected` strategy (zccache added the flags purely for its own
/// depgraph use; the user never asked for the file on disk) and for
/// compilers that don't support depfiles.
///
/// Used on the cache-hit path (issue #643): the cached artifact carries
/// the original depfile bytes as a second output, and this function tells
/// the hit-materializer where to write those bytes in the *current*
/// build. The cached payload's name is just a stored identifier;
/// the on-disk destination is always derived from the current request.
#[must_use]
pub fn user_depfile_destination(
    dep_flags: &UserDepFlags,
    output_file: &Path,
) -> Option<NormalizedPath> {
    if let Some(ref mf_path) = dep_flags.mf_path {
        return Some(mf_path.clone());
    }
    if dep_flags.has_md {
        return Some(output_file.with_extension("d").into());
    }
    None
}

/// Determine depfile strategy and return extra args to append to the compiler.
///
/// `supports_depfile`: whether the compiler family supports `-MD -MF`.
/// `dep_flags`: user's existing dependency flags.
/// `output_file`: the `-o` output file path (used to derive default `.d` path).
/// `tmpdir`: directory for injected depfiles.
///
/// Returns `(extra_args, strategy)`. `extra_args` is empty unless we inject flags.
pub fn prepare_depfile(
    supports_depfile: bool,
    dep_flags: &UserDepFlags,
    output_file: &Path,
    tmpdir: &Path,
) -> (Vec<String>, DepfileStrategy) {
    if !supports_depfile {
        return (Vec::new(), DepfileStrategy::Unsupported);
    }

    // User already specified -MF <path>: use their file.
    if let Some(ref mf_path) = dep_flags.mf_path {
        return (
            Vec::new(),
            DepfileStrategy::UserSpecified {
                path: mf_path.clone(),
            },
        );
    }

    // User has -MD/-MMD but no -MF: derive from output file stem.
    if dep_flags.has_md {
        let d_path = output_file.with_extension("d");
        return (
            Vec::new(),
            DepfileStrategy::UserDefault {
                path: d_path.into(),
            },
        );
    }

    // No user dep flags: inject -MD -MF <tmpfile>.
    // Re-create tmpdir if it was deleted (e.g. by Windows temp cleanup)
    // while the daemon is still running. Without this, the compiler fails
    // with "error opening ... no such file or directory".
    if !tmpdir.exists() {
        let _ = std::fs::create_dir_all(tmpdir);
    }
    static DEPFILE_COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = DEPFILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let stem = output_file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("depfile");
    let tmp_path = tmpdir.join(format!("{stem}_{}_{unique}.d", std::process::id()));
    let tmp_path: NormalizedPath = tmp_path.into();
    let extra_args = vec![
        "-MD".to_string(),
        "-MF".to_string(),
        tmp_path.to_string_lossy().into_owned(),
    ];
    (extra_args, DepfileStrategy::Injected { path: tmp_path })
}

//! `zccache meson configure` — cache-aware `meson setup` wrapper.
//!
//! Issue #627. Implements the configure-cache sketch from the issue body:
//! on a cold invocation we shell out to real `meson setup`, capture the
//! resulting build directory contents into a zccache-rooted artifact
//! tree, and on subsequent invocations with identical inputs we restore
//! the cached contents and skip meson entirely.
//!
//! The user-facing argument here is the build directory PATH, not its
//! content. Build-dir-portable caching would require rewriting the
//! absolute paths meson scatters through `meson-info/` and
//! `meson-private/` on materialisation (see #627 open question 2). For
//! the common dev-loop case (stable build dir per developer) this is
//! unnecessary, and a CI matrix that uses distinct build dirs per
//! platform still benefits as soon as each tuple has run once.
//!
//! **Cache key inputs** (blake3, domain-separated):
//!
//! - The `[meson.build, meson.options, meson_options.txt]` file set
//!   discovered recursively under the source dir, each file's content
//!   hashed and the (relative-path, content-hash) pairs sorted.
//!   Pass `--no-walk` to skip this implicit discovery — the caller then
//!   takes full responsibility for naming every input file via
//!   `--input-file` (intended for monorepos whose default skip list
//!   doesn't cover their scratch dirs; see issue #659).
//! - The meson executable's `--version` output (cheap, stable).
//! - The build-directory **absolute path** (same-build-dir restriction).
//! - The source-directory **absolute path** (so a renamed source tree
//!   gets a fresh entry — meson embeds the source path in `build.ninja`).
//! - Selected environment variables: `CC`, `CXX`, `CFLAGS`, `CXXFLAGS`,
//!   `LDFLAGS`, `PKG_CONFIG_PATH` always; plus any extras supplied via
//!   `--input-env`. Each as `NAME=VALUE` with absent vars contributing
//!   `NAME=` (so set-vs-unset distinguishes).
//! - Any extra `--input-file PATH` flags. Each file is read and its
//!   content hashed alongside the meson.build set; the path is recorded
//!   verbatim as it appeared on the command line. Repeats are
//!   deduplicated and the set is sorted before hashing, so argv order
//!   is irrelevant. Intended for downstream layers whose source-change
//!   detection lives outside `meson.build` (e.g. a digest-of-globs
//!   sidecar file). See issue #654.
//! - The verbatim `meson_args` trailing argv (so different `-Dopt=…`
//!   combinations produce distinct entries).
//! - A version tag (`"zccache-meson-cache-v2"`) for forward-compat key
//!   rotation when the capture/restore scheme evolves. Bumped from
//!   `v1` -> `v2` alongside the `--input-file` addition so existing
//!   cache entries don't collide with the new key layout.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use crate::core::config;

/// Cache-key version. **v3 bump (issue #710)** invalidates v2 entries that
/// had captured the *entire* build dir (including built `*.pch`, `*.obj`,
/// downstream tool sidecars), because restoring those into a fresh build
/// dir produced stale PCHs that silently defeated `#pragma once` dedup and
/// poisoned every subsequent build. v3 entries are captured under a strict
/// allowlist (`is_configure_output`) of meson-owned configure outputs only.
const KEY_DOMAIN_TAG: &str = "zccache-meson-cache-v3";

/// Environment variables whose values always feed the cache key. Set
/// regardless of `--input-env` so a forgotten extra never silently
/// publishes a stale cache hit.
const DEFAULT_INPUT_ENV: &[&str] = &[
    "CC",
    "CXX",
    "CFLAGS",
    "CXXFLAGS",
    "LDFLAGS",
    "PKG_CONFIG_PATH",
];

/// Filenames meson reads as configure inputs. Recursively discovered
/// under the source dir; each matching file's content enters the key.
const MESON_INPUT_FILENAMES: &[&str] = &["meson.build", "meson.options", "meson_options.txt"];

pub(crate) fn cmd_configure(
    source_dir: PathBuf,
    build_dir: PathBuf,
    meson_bin: Option<PathBuf>,
    extra_input_env: Vec<String>,
    extra_input_file: Vec<String>,
    no_walk: bool,
    meson_args: Vec<String>,
) -> ExitCode {
    if !source_dir.exists() {
        eprintln!(
            "[zccache-meson] error: source dir {} does not exist",
            source_dir.display()
        );
        return ExitCode::FAILURE;
    }
    // Absolutise without `canonicalize` — on Windows canonicalize injects
    // the `\\?\` UNC prefix that subprocesses generally don't like.
    let source_abs = absolutise(&source_dir);
    let build_abs = absolutise(&build_dir);
    let meson_bin = meson_bin.unwrap_or_else(|| PathBuf::from("meson"));

    let meson_version = match capture_meson_version(&meson_bin) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[zccache-meson] error: cannot read `meson --version`: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut env_inputs: Vec<&str> = DEFAULT_INPUT_ENV.to_vec();
    for extra in &extra_input_env {
        if !env_inputs.iter().any(|s| s == extra) {
            env_inputs.push(extra.as_str());
        }
    }
    env_inputs.sort_unstable();

    let env_pairs: Vec<(String, String)> = env_inputs
        .iter()
        .map(|name| ((*name).to_string(), std::env::var(name).unwrap_or_default()))
        .collect();

    // With `--no-walk` the caller takes full responsibility for naming
    // every input file via `--input-file`. The implicit walk of the
    // source directory is skipped entirely (no traversal, no read), and
    // the empty discovery set is the documented signal — distinct from
    // the "walked but found nothing" error path below.
    let input_files = if no_walk {
        BTreeMap::new()
    } else {
        match discover_meson_inputs(&source_abs) {
            Ok(set) => set,
            Err(e) => {
                eprintln!("[zccache-meson] error: scanning source dir failed: {e}");
                return ExitCode::FAILURE;
            }
        }
    };

    if !no_walk && input_files.is_empty() {
        eprintln!(
            "[zccache-meson] error: no meson input files found under {}",
            source_abs.display()
        );
        return ExitCode::FAILURE;
    }

    // With `--no-walk` the caller must supply at least one `--input-file`
    // — otherwise the cache key only sees the (source, build, version,
    // env, args) tuple, which is rarely what anyone wants.
    if no_walk && extra_input_file.is_empty() {
        eprintln!(
            "[zccache-meson] error: --no-walk requires at least one --input-file (otherwise the cache key has no source-content contribution)"
        );
        return ExitCode::FAILURE;
    }

    let key_hex = match compute_cache_key(
        &input_files,
        &source_abs,
        &build_abs,
        &meson_version,
        &env_pairs,
        &extra_input_file,
        &meson_args,
    ) {
        Ok(hex) => hex,
        Err(e) => {
            eprintln!("[zccache-meson] error: hashing inputs failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    let cache_root = config::default_cache_dir();
    let entry_dir = Path::new(cache_root.as_path())
        .join("meson-configure")
        .join(&key_hex);
    let payload_path = entry_dir.join("build-dir.tar");
    let stdout_path = entry_dir.join("stdout.bin");
    let stderr_path = entry_dir.join("stderr.bin");

    if payload_path.exists() {
        match restore_from_cache(&payload_path, &stdout_path, &stderr_path, &build_abs) {
            Ok(()) => {
                eprintln!(
                    "[zccache-meson] hit key={} build_dir={}",
                    &key_hex[..16],
                    build_abs.display()
                );
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!(
                    "[zccache-meson] warning: cache restore failed ({e}); falling back to fresh meson setup"
                );
                // Fall through to miss path.
            }
        }
    }

    eprintln!(
        "[zccache-meson] miss key={} build_dir={}",
        &key_hex[..16],
        build_abs.display()
    );

    // Miss: run real meson. The build dir is created by meson itself.
    let _ = std::fs::create_dir_all(&build_abs);
    let mut cmd = Command::new(&meson_bin);
    cmd.arg("setup").arg(&build_abs).arg(&source_abs);
    for arg in &meson_args {
        cmd.arg(arg);
    }
    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!(
                "[zccache-meson] error: failed to run `{} setup`: {e}",
                meson_bin.display()
            );
            return ExitCode::FAILURE;
        }
    };

    // Replay meson's output to the caller's streams regardless of hit/miss.
    let _ = std::io::Write::write_all(&mut std::io::stdout(), &output.stdout);
    let _ = std::io::Write::write_all(&mut std::io::stderr(), &output.stderr);

    if !output.status.success() {
        // meson failed — DO NOT cache. The user sees meson's exit code
        // unchanged and can re-run after fixing the issue. Caching the
        // failed config would defeat the whole point: a re-run would
        // restore the failure instead of giving meson a fresh chance.
        return crate::cli::commands::util::exit_code_from_i32(output.status.code().unwrap_or(1));
    }

    // Skip persist on the "no-op reconfigure" path (issue #710). When meson
    // prints "Directory already configured." the build dir is the *prior*
    // build's full tree (object files, PCHs, downstream sidecars), not
    // something meson produced this invocation. Persisting that state
    // poisoned future restores. Emit a hint instead and return success.
    if stdout_contains_already_configured(&output.stdout) {
        eprintln!(
            "[zccache-meson] skip persist key={} build_dir={} reason=already-configured (issue #710)",
            &key_hex[..16],
            build_abs.display()
        );
        return ExitCode::SUCCESS;
    }

    if let Err(e) = persist_to_cache(
        &entry_dir,
        &payload_path,
        &stdout_path,
        &stderr_path,
        &output.stdout,
        &output.stderr,
        &build_abs,
    ) {
        // Persist failure on a successful meson is non-fatal — the
        // user got a valid build dir; they just don't get the cache
        // win for next time. Surface the warning so it's diagnosable.
        eprintln!("[zccache-meson] warning: failed to persist cache entry: {e}");
    }

    ExitCode::SUCCESS
}

fn absolutise(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    }
}

fn capture_meson_version(meson_bin: &Path) -> std::io::Result<String> {
    let out = Command::new(meson_bin).arg("--version").output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "`{} --version` exit={}",
            meson_bin.display(),
            out.status
        )));
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(s)
}

/// Discover every meson input file recursively under `root`. Returns a
/// map keyed by the path *relative to root* (forward-slash normalised)
/// so the resulting cache key is portable across path-separator
/// quirks. The file content itself is read in
/// [`compute_cache_key`] — keeping discovery and hashing separate so
/// the discovery list can also drive the "is this list stable?" debug
/// output if added later.
fn discover_meson_inputs(root: &Path) -> std::io::Result<BTreeMap<String, PathBuf>> {
    let mut out = BTreeMap::new();
    walk_meson_dir(root, root, &mut out)?;
    Ok(out)
}

fn walk_meson_dir(
    root: &Path,
    dir: &Path,
    acc: &mut BTreeMap<String, PathBuf>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            // Skip common scratch / dependency / VCS directories. These
            // are the dirs every real project has in tree that meson
            // would never legitimately read configure inputs from. The
            // list is intentionally conservative — only directories
            // whose ENTIRE canonical purpose is "out of source-control
            // build/cache state". Callers that need exact control over
            // which dirs are walked (or want to skip the walk entirely)
            // should use `--no-walk` plus explicit `--input-file`
            // entries — see issue #659.
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if matches!(
                    name,
                    ".git"
                        | ".hg"
                        | ".svn"
                        | "build"
                        | ".build"
                        | "target"
                        | "node_modules"
                        | ".venv"
                        | "venv"
                        | ".cargo"
                ) {
                    continue;
                }
            }
            walk_meson_dir(root, &path, acc)?;
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !MESON_INPUT_FILENAMES.contains(&name) {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        acc.insert(rel, path);
    }
    Ok(())
}

fn compute_cache_key(
    inputs: &BTreeMap<String, PathBuf>,
    source_abs: &Path,
    build_abs: &Path,
    meson_version: &str,
    env_pairs: &[(String, String)],
    extra_input_files: &[String],
    meson_args: &[String],
) -> std::io::Result<String> {
    let mut hasher = blake3::Hasher::new_derive_key(KEY_DOMAIN_TAG);
    // Source and build dir absolute paths — same-build-dir restriction.
    hash_str_with_tag(&mut hasher, "source", &source_abs.to_string_lossy());
    hash_str_with_tag(&mut hasher, "build", &build_abs.to_string_lossy());
    // meson version string (e.g. "1.5.1").
    hash_str_with_tag(&mut hasher, "meson-version", meson_version);
    // Sorted env pairs (set-vs-unset captured by the empty-string convention).
    hasher.update(b"env\0");
    for (k, v) in env_pairs {
        hasher.update(k.as_bytes());
        hasher.update(b"=");
        hasher.update(v.as_bytes());
        hasher.update(b"\0");
    }
    // Trailing meson_args verbatim.
    hasher.update(b"args\0");
    for a in meson_args {
        hasher.update(a.as_bytes());
        hasher.update(b"\0");
    }
    // meson.build et al. content. Ordered by relative path (BTreeMap).
    hasher.update(b"inputs\0");
    for (rel, abs) in inputs {
        let bytes = std::fs::read(abs)?;
        hasher.update(rel.as_bytes());
        hasher.update(b"\0");
        hasher.update(&bytes);
        hasher.update(b"\0");
    }
    // User-supplied extra input files (issue #654). Each file's path is
    // recorded as it appeared on the command line — sorted by that path
    // so duplicate `--input-file foo --input-file foo` is idempotent and
    // the key is stable across argv reordering. The empty list is a no-op
    // and produces the same key contribution every time (just the
    // section delimiter), so callers that don't pass `--input-file` see
    // no behavior change beyond the v1 → v2 domain tag bump.
    hasher.update(b"extra-inputs\0");
    let mut sorted: Vec<&String> = extra_input_files.iter().collect();
    sorted.sort();
    sorted.dedup();
    for path in sorted {
        let bytes = std::fs::read(path)
            .map_err(|e| std::io::Error::new(e.kind(), format!("--input-file {path}: {e}")))?;
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update(&bytes);
        hasher.update(b"\0");
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn hash_str_with_tag(hasher: &mut blake3::Hasher, tag: &str, value: &str) {
    hasher.update(tag.as_bytes());
    hasher.update(b"=");
    hasher.update(value.as_bytes());
    hasher.update(b"\0");
}

/// Persist a freshly-produced build dir into the cache. The build dir
/// contents are walked, each file written as a record in a simple
/// length-prefixed tarball format: `[u32 path_len][path bytes][u64
/// content_len][content bytes]` repeated until EOF.
///
/// Why hand-roll a format instead of using `tar`: the `tar` crate brings
/// in its own modtime / uid / gid handling that's a poor fit for a
/// content-only cache where mtimes are explicitly NOT preserved (the
/// restore writes everything with `now()` so meson's regen-trigger logic
/// re-validates inputs but doesn't think the cache files were
/// pre-existing). The format is tiny, well-defined, and trivial to
/// extend.
fn persist_to_cache(
    entry_dir: &Path,
    payload_path: &Path,
    stdout_path: &Path,
    stderr_path: &Path,
    stdout_bytes: &[u8],
    stderr_bytes: &[u8],
    build_abs: &Path,
) -> std::io::Result<()> {
    std::fs::create_dir_all(entry_dir)?;
    let tmp = payload_path.with_extension("tar.tmp");
    {
        let f = std::fs::File::create(&tmp)?;
        let mut writer = std::io::BufWriter::new(f);
        archive_dir(build_abs, build_abs, &mut writer)?;
        std::io::Write::flush(&mut writer)?;
    }
    std::fs::rename(&tmp, payload_path)?;
    std::fs::write(stdout_path, stdout_bytes)?;
    std::fs::write(stderr_path, stderr_bytes)?;
    Ok(())
}

/// Strict allowlist of paths relative to the build root that are meson's
/// own configure outputs (issue #710). Anything else — `*.obj`, `*.pch`,
/// `.input_hash`, target subdirs, etc. — must not enter the snapshot,
/// because restoring those into a fresh build dir produces stale-file
/// poisoning that silently defeats `#pragma once` dedup.
///
/// `rel` is forward-slash-normalised.
fn is_configure_output(rel: &str) -> bool {
    matches!(rel, "build.ninja" | "compile_commands.json")
        || rel.starts_with("meson-info/")
        || rel.starts_with("meson-private/")
        || rel.starts_with("meson-logs/")
}

/// Allowlist of directories the archive walker is allowed to recurse into.
/// Mirrors `is_configure_output` at the directory level so we never read
/// (let alone capture) entries under `tests/`, `subprojects/<sub>/build/`,
/// etc.
fn is_configure_output_dir(rel: &str) -> bool {
    matches!(rel, "meson-info" | "meson-private" | "meson-logs")
        || rel.starts_with("meson-info/")
        || rel.starts_with("meson-private/")
        || rel.starts_with("meson-logs/")
}

fn archive_dir(
    root: &Path,
    dir: &Path,
    writer: &mut std::io::BufWriter<std::fs::File>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if path.is_dir() {
            if is_configure_output_dir(&rel) {
                archive_dir(root, &path, writer)?;
            }
            continue;
        }
        if !is_configure_output(&rel) {
            continue;
        }
        let rel_bytes = rel.as_bytes();
        let content = std::fs::read(&path)?;
        use std::io::Write;
        writer.write_all(&(rel_bytes.len() as u32).to_le_bytes())?;
        writer.write_all(rel_bytes)?;
        writer.write_all(&(content.len() as u64).to_le_bytes())?;
        writer.write_all(&content)?;
    }
    Ok(())
}

fn restore_from_cache(
    payload_path: &Path,
    stdout_path: &Path,
    stderr_path: &Path,
    build_abs: &Path,
) -> std::io::Result<()> {
    // Ensure a clean restore destination. If the build dir already has
    // partial contents (e.g. a previous interrupted miss), wipe so the
    // restore is bit-identical to the cold-run snapshot.
    if build_abs.exists() {
        std::fs::remove_dir_all(build_abs)?;
    }
    std::fs::create_dir_all(build_abs)?;

    let f = std::fs::File::open(payload_path)?;
    let mut reader = std::io::BufReader::new(f);
    loop {
        use std::io::Read;
        let mut len_buf = [0u8; 4];
        match reader.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
        let path_len = u32::from_le_bytes(len_buf) as usize;
        let mut path_bytes = vec![0u8; path_len];
        reader.read_exact(&mut path_bytes)?;
        let rel = String::from_utf8(path_bytes).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("non-UTF8 path in archive: {e}"),
            )
        })?;

        let mut content_len_buf = [0u8; 8];
        reader.read_exact(&mut content_len_buf)?;
        let content_len = u64::from_le_bytes(content_len_buf) as usize;
        let mut content = vec![0u8; content_len];
        reader.read_exact(&mut content)?;

        let dest = build_abs.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, &content)?;
    }

    // Replay the cached stdout/stderr so the caller sees the same
    // operator-facing output a cold run would have produced.
    if let Ok(bytes) = std::fs::read(stdout_path) {
        let _ = std::io::Write::write_all(&mut std::io::stdout(), &bytes);
    }
    if let Ok(bytes) = std::fs::read(stderr_path) {
        let _ = std::io::Write::write_all(&mut std::io::stderr(), &bytes);
    }
    Ok(())
}

/// Detect meson's no-op-reconfigure signal in stdout (issue #710).
fn stdout_contains_already_configured(stdout: &[u8]) -> bool {
    const NEEDLE: &[u8] = b"Directory already configured.";
    stdout.windows(NEEDLE.len()).any(|w| w == NEEDLE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_keeps_meson_configure_outputs() {
        assert!(is_configure_output("build.ninja"));
        assert!(is_configure_output("compile_commands.json"));
        assert!(is_configure_output("meson-info/intro-targets.json"));
        assert!(is_configure_output("meson-private/coredata.dat"));
        assert!(is_configure_output("meson-logs/meson-log.txt"));
    }

    #[test]
    fn allowlist_rejects_build_outputs_and_pch_sidecars() {
        // Issue #710 reproducer paths from the FastLED build dir:
        assert!(!is_configure_output("tests/test_pch.h.pch"));
        assert!(!is_configure_output("tests/test_pch.h.pch.input_hash"));
        assert!(!is_configure_output("tests/test_pch.h.d.cache"));
        // Generic ninja outputs:
        assert!(!is_configure_output("tests/foo.obj"));
        assert!(!is_configure_output("tests/foo.o"));
        assert!(!is_configure_output("libfastled.a"));
        assert!(!is_configure_output("libfastled.dll"));
        assert!(!is_configure_output("libfastled.so"));
        assert!(!is_configure_output("test_pch.exe"));
        assert!(!is_configure_output("test_pch.pdb"));
        // Subdirs that ninja owns:
        assert!(!is_configure_output("examples/Blink/Blink.dll"));
        assert!(!is_configure_output("subprojects/lib/whatever.obj"));
        // Ninja state — not configure output:
        assert!(!is_configure_output(".ninja_log"));
        assert!(!is_configure_output(".ninja_deps"));
    }

    #[test]
    fn dir_allowlist_lets_walker_skip_target_subdirs() {
        assert!(is_configure_output_dir("meson-info"));
        assert!(is_configure_output_dir("meson-private"));
        assert!(is_configure_output_dir("meson-logs"));
        assert!(is_configure_output_dir("meson-info/sub"));
        assert!(!is_configure_output_dir("tests"));
        assert!(!is_configure_output_dir("examples"));
        assert!(!is_configure_output_dir("subprojects"));
        assert!(!is_configure_output_dir("CMakeFiles"));
    }

    #[test]
    fn no_op_reconfigure_detected_in_stdout() {
        let sample =
            b"The Meson build system\nVersion: 1.6.0\nDirectory already configured.\n\nJust run your build command (e.g. ninja) and Meson will regenerate as necessary.\n";
        assert!(stdout_contains_already_configured(sample));
    }

    #[test]
    fn no_op_detection_does_not_false_positive_on_normal_configure() {
        let normal = b"The Meson build system\nVersion: 1.6.0\nSource dir: /tmp/src\nBuild dir: /tmp/build\nBuild type: native build\n";
        assert!(!stdout_contains_already_configured(normal));
    }

    #[test]
    fn archive_dir_only_captures_allowlisted_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Configure outputs that SHOULD be captured:
        std::fs::write(root.join("build.ninja"), b"# ninja").unwrap();
        std::fs::write(root.join("compile_commands.json"), b"[]").unwrap();
        std::fs::create_dir_all(root.join("meson-info")).unwrap();
        std::fs::write(root.join("meson-info/intro-targets.json"), b"[]").unwrap();
        std::fs::create_dir_all(root.join("meson-private")).unwrap();
        std::fs::write(root.join("meson-private/coredata.dat"), b"\0\0").unwrap();
        std::fs::create_dir_all(root.join("meson-logs")).unwrap();
        std::fs::write(root.join("meson-logs/meson-log.txt"), b"ok").unwrap();

        // Poison the dir with the actual #710 reproducer files — these
        // MUST be skipped:
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("tests/test_pch.h.pch"), b"STALEPCH").unwrap();
        std::fs::write(root.join("tests/test_pch.h.pch.input_hash"), b"deadbeef").unwrap();
        std::fs::write(root.join("tests/test_pch.h.d.cache"), b"depcache").unwrap();
        std::fs::write(root.join("tests/foo.obj"), b"OBJECTBYTES").unwrap();
        std::fs::create_dir_all(root.join("examples/Blink")).unwrap();
        std::fs::write(root.join("examples/Blink/Blink.dll"), b"DLLBYTES").unwrap();
        std::fs::write(root.join(".ninja_log"), b"log").unwrap();

        let tar_path = tmp.path().join("out.tar");
        {
            let f = std::fs::File::create(&tar_path).unwrap();
            let mut writer = std::io::BufWriter::new(f);
            archive_dir(root, root, &mut writer).unwrap();
            std::io::Write::flush(&mut writer).unwrap();
        }

        // Read back the archive and collect captured rel paths.
        let mut captured: Vec<String> = Vec::new();
        let f = std::fs::File::open(&tar_path).unwrap();
        let mut reader = std::io::BufReader::new(f);
        loop {
            use std::io::Read;
            let mut len_buf = [0u8; 4];
            match reader.read_exact(&mut len_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => panic!("read failed: {e}"),
            }
            let path_len = u32::from_le_bytes(len_buf) as usize;
            let mut path_bytes = vec![0u8; path_len];
            reader.read_exact(&mut path_bytes).unwrap();
            let rel = String::from_utf8(path_bytes).unwrap();
            let mut content_len_buf = [0u8; 8];
            reader.read_exact(&mut content_len_buf).unwrap();
            let content_len = u64::from_le_bytes(content_len_buf) as usize;
            let mut content = vec![0u8; content_len];
            reader.read_exact(&mut content).unwrap();
            captured.push(rel);
        }
        captured.sort();

        // Everything captured must be configure output.
        for rel in &captured {
            assert!(
                is_configure_output(rel),
                "archive captured a non-configure path: {rel}"
            );
        }
        // None of the #710 poison files made it in.
        for poison in &[
            "tests/test_pch.h.pch",
            "tests/test_pch.h.pch.input_hash",
            "tests/test_pch.h.d.cache",
            "tests/foo.obj",
            "examples/Blink/Blink.dll",
            ".ninja_log",
        ] {
            assert!(
                !captured.iter().any(|r| r == poison),
                "archive captured poison file: {poison}"
            );
        }
        // And the real configure outputs made it in.
        for needed in &[
            "build.ninja",
            "compile_commands.json",
            "meson-info/intro-targets.json",
            "meson-private/coredata.dat",
            "meson-logs/meson-log.txt",
        ] {
            assert!(
                captured.iter().any(|r| r == needed),
                "archive missed required configure output: {needed}"
            );
        }
    }
}

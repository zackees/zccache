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
//! - The meson executable's `--version` output (cheap, stable).
//! - The build-directory **absolute path** (same-build-dir restriction).
//! - The source-directory **absolute path** (so a renamed source tree
//!   gets a fresh entry — meson embeds the source path in `build.ninja`).
//! - Selected environment variables: `CC`, `CXX`, `CFLAGS`, `CXXFLAGS`,
//!   `LDFLAGS`, `PKG_CONFIG_PATH` always; plus any extras supplied via
//!   `--input-env`. Each as `NAME=VALUE` with absent vars contributing
//!   `NAME=` (so set-vs-unset distinguishes).
//! - The verbatim `meson_args` trailing argv (so different `-Dopt=…`
//!   combinations produce distinct entries).
//! - A version tag (`"zccache-meson-cache-v1"`) for forward-compat key
//!   rotation when the capture/restore scheme evolves.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use crate::core::config;

const KEY_DOMAIN_TAG: &str = "zccache-meson-cache-v1";

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

    let input_files = match discover_meson_inputs(&source_abs) {
        Ok(set) => set,
        Err(e) => {
            eprintln!("[zccache-meson] error: scanning source dir failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    if input_files.is_empty() {
        eprintln!(
            "[zccache-meson] error: no meson input files found under {}",
            source_abs.display()
        );
        return ExitCode::FAILURE;
    }

    let key_hex = match compute_cache_key(
        &input_files,
        &source_abs,
        &build_abs,
        &meson_version,
        &env_pairs,
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
            // Skip common scratch dirs that meson doesn't read but might
            // contain copies of meson.build inside vendored sources.
            // Keep the list short and conservative.
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if matches!(
                    name,
                    "build" | ".build" | "target" | "node_modules" | ".git"
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

fn archive_dir(
    root: &Path,
    dir: &Path,
    writer: &mut std::io::BufWriter<std::fs::File>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            archive_dir(root, &path, writer)?;
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
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

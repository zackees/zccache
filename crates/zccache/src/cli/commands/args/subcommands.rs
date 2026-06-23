//! Per-subcommand `clap::Subcommand` enums and value-enums referenced
//! from `Commands` in the parent module.
//!
//! Kept in a sibling file purely to keep `args/mod.rs` under the 1,000-LOC
//! cap. The public path (`cli::commands::args::<Name>`) is preserved via
//! re-exports in `args/mod.rs`.

use clap::{Subcommand, ValueEnum};
use std::path::PathBuf;

/// `zccache daemon` subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum DaemonCommands {
    /// Restart the daemon with Tokio Console instrumentation enabled.
    Top {
        /// Tokio Console bind address, e.g. localhost:1234.
        bind: Option<String>,
        /// Tokio Console bind address, e.g. localhost:1234.
        #[arg(long = "bind")]
        bind_addr: Option<String>,
        /// Do not launch the `tokio-console` terminal UI after the daemon is ready.
        #[arg(long)]
        no_open: bool,
        /// IPC endpoint for the zccache daemon itself (default: platform-specific).
        #[arg(long)]
        endpoint: Option<String>,
    },
    /// Compatibility alias for `zccache daemon top`.
    #[command(hide = true)]
    Profile {
        #[command(subcommand)]
        command: DaemonProfileCommands,
    },
}

/// `zccache daemon profile` subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum DaemonProfileCommands {
    /// Compatibility alias for `zccache daemon top`.
    Start {
        /// Tokio Console bind address, e.g. localhost:1234.
        #[arg(long)]
        bind: Option<String>,
        /// Launch the `tokio-console` terminal UI after the daemon is ready.
        #[arg(long)]
        open: bool,
        /// IPC endpoint for the zccache daemon itself (default: platform-specific).
        #[arg(long)]
        endpoint: Option<String>,
    },
}

/// `zccache cache` subcommands (#695).
#[derive(Debug, Subcommand)]
pub(crate) enum CacheCommands {
    /// Report the total on-disk size of the resolved cache root.
    ///
    /// Walks the same root that `zccache cache-root` prints and sums the
    /// bytes of every regular file under it. Hardlinks are counted once
    /// (deduplicated by `(dev, inode)` on Unix; on Windows by inode-like
    /// identity when available). Prints `<bytes>\t<human>\t<root>` on a
    /// single line by default; use `--json` for a structured emit.
    Size {
        /// Emit `{"bytes": N, "human": "X GiB", "cache_root": "<abs>"}` instead of the
        /// plain line.
        #[arg(long)]
        json: bool,
    },
    /// List per-version cache directories visible under the resolved cache
    /// root, with last-active time, size, and status (current/warm/cold).
    ///
    /// Today's single-root cache reports a single row (the resolved root
    /// labeled `current`); when the multi-version `~/.zccache/v-<version>/`
    /// layout from #694 lands, each per-version directory shows up as its
    /// own row without any further CLI changes.
    List {
        /// Emit a JSON array of `{version, status, size_bytes, last_active_unix, path}`
        /// instead of the plain tabular output.
        #[arg(long)]
        json: bool,
    },
}

/// `zccache meson` subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum MesonCommands {
    /// Cache-aware `meson setup`. On first invocation runs real meson and
    /// captures the resulting build directory into the zccache cache; on
    /// subsequent invocations with the same `meson.build` set, environment,
    /// and meson version, restores the cached build directory and skips
    /// invoking meson entirely.
    ///
    /// **Same-build-dir restriction** — the build directory path is part of
    /// the cache key. Build-dir-portable caching would require rewriting
    /// the absolute paths meson scatters through `meson-info/` and
    /// `meson-private/` on materialisation; for the common dev-loop case
    /// (one developer, stable build dir) this is unnecessary. The
    /// substantially larger fastled-style CI matrix that uses different
    /// build dirs per platform still benefits because each (source, build,
    /// host, env) tuple converges to a per-tuple cache entry on its second
    /// invocation.
    Configure {
        /// Project source directory containing the root `meson.build`.
        #[arg(long = "source-dir", value_name = "DIR")]
        source_dir: PathBuf,
        /// Build directory meson will populate.
        #[arg(long = "build-dir", value_name = "DIR")]
        build_dir: PathBuf,
        /// Path to the meson executable. Defaults to `meson` on PATH.
        #[arg(long = "meson-bin", value_name = "PATH")]
        meson_bin: Option<PathBuf>,
        /// Extra environment variable names whose values feed the cache
        /// key. The current process env is queried at request time.
        /// Repeatable. Common defaults (CC, CXX, CFLAGS, CXXFLAGS,
        /// LDFLAGS, PKG_CONFIG_PATH) are always included.
        #[arg(long = "input-env", value_name = "NAME")]
        input_env: Vec<String>,
        /// Extra file paths whose content feeds the cache key. Repeatable.
        /// Each file is hashed by content; the path is recorded as it
        /// appeared on the command line.
        ///
        /// Use this when source-change detection lives outside the
        /// meson.build set itself — e.g. a downstream caching layer that
        /// hashes test/example/source globs and writes the digest to a
        /// sidecar file. Pointing `--input-file` at that sidecar lets the
        /// wrapper invalidate the cached configure tree when those globs
        /// change, instead of forcing the caller to bypass the wrapper
        /// entirely. See issue #654.
        #[arg(long = "input-file", value_name = "PATH")]
        input_file: Vec<String>,
        /// Skip the implicit recursive walk of `--source-dir` for
        /// `meson.build` / `meson.options` / `meson_options.txt`. The
        /// caller takes full responsibility for naming every input file
        /// via `--input-file` instead. Use this when you know your
        /// project's input set exactly and the implicit walk is paying
        /// for itself in directory-traversal cost on large monorepos
        /// (e.g. trees with `.venv`, `.cached`, `.fbuild`, `.pio`, or
        /// other large scratch dirs not on the default skip list). See
        /// issue #659.
        #[arg(long = "no-walk", default_value_t = false)]
        no_walk: bool,
        /// Extra `meson setup` arguments passed verbatim on a miss. They
        /// also enter the cache key so different option sets produce
        /// distinct cache entries.
        #[arg(trailing_var_arg = true)]
        meson_args: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum SymbolsCommands {
    /// Download the matching `-debug` archive from the GitHub release and
    /// drop the per-binary sidecars next to the running zccache executable
    /// so cdb/WinDbg (or perf/lldb) can resolve symbols.
    Install {
        /// Override the release version to fetch (defaults to the running
        /// binary's compile-time `CARGO_PKG_VERSION`).
        #[arg(long)]
        version: Option<String>,
        /// Override the Rust target triple (defaults to the running binary's
        /// compile-time `ZCCACHE_BUILD_TARGET`).
        #[arg(long)]
        target: Option<String>,
        /// Install into this directory instead of the directory containing
        /// the running zccache executable.
        #[arg(long)]
        prefix: Option<PathBuf>,
        /// Re-download even if matching sidecars are already present.
        #[arg(long)]
        force: bool,
    },
    /// Resolve symbols for one or more crash dumps. Reads the release
    /// marker appended to the running zccache binary to determine which
    /// version + target's symbol archive to fetch; the archive is cached
    /// under `<cache>/symbols/<v>-<triple>/` (one copy per build) and a
    /// `<dump>.symref` sidecar is written next to each crash dump
    /// pointing at the cached symbol directory. If the running binary
    /// is a dev build (no release marker), reports that and exits — use
    /// the local `target/release/*.{pdb,dwp,dSYM}` manually.
    Symbolicate {
        /// One or more crash dump paths (`crash-*.txt` or `crash-*.dmp`).
        #[arg(required = true)]
        dumps: Vec<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum DefenderExclusionsCommands {
    /// Print whether the resolved cache root (and any sibling `runtime/`)
    /// is on Defender's exclusion list. Non-destructive — no elevation
    /// needed. Use `--json` for machine-readable output.
    Check {
        /// Emit a JSON document on stdout instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Add the cache root (and any sibling `runtime/`) to Defender's
    /// exclusion list. Requires administrator elevation; exits non-zero
    /// with instructions when run from a non-elevated shell.
    Add,
    /// Remove the cache root (and any sibling `runtime/`) from Defender's
    /// exclusion list. Requires administrator elevation.
    Remove,
}

#[derive(Debug, Subcommand)]
pub(crate) enum CargoRegistryCommands {
    /// Save cargo registry to a compressed archive.
    Save {
        /// Cache key (used as filename when `--output` is not set).
        #[arg(long)]
        key: String,
        /// Cargo home directory (default: ~/.cargo or $CARGO_HOME).
        #[arg(long)]
        cargo_home: Option<String>,
        /// Explicit output archive path. When supplied, the archive is
        /// written exactly here instead of the default
        /// `<cache-root>/cargo-registry/<key>.tar.gz`, AND the
        /// `SOLDR_SKIP_CARGO_REGISTRY_SAVE=1` no-op is bypassed —
        /// the caller has chosen a non-standard destination, so the
        /// setup-soldr coordination flag (which exists to avoid
        /// double-saving the standard location) does not apply.
        #[arg(long)]
        output: Option<String>,
    },
    /// Restore cargo registry from a compressed archive.
    Restore {
        /// Cache key to restore.
        #[arg(long)]
        key: String,
        /// Cargo home directory (default: ~/.cargo or $CARGO_HOME).
        #[arg(long)]
        cargo_home: Option<String>,
    },
    /// Print hash of Cargo.lock for use as cache key.
    Hash {
        /// Path to Cargo.lock (default: ./Cargo.lock).
        #[arg(long, default_value = "Cargo.lock")]
        lockfile: String,
    },
    /// Remove cached registry archives.
    Clean,
}

/// `zccache kv` subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum KvCommands {
    /// Read a value to stdout. Exit 2 if missing.
    Get {
        /// Namespace (`[a-z0-9-]{1,64}`).
        namespace: String,
        /// 64-char hex key.
        hex_key: String,
    },
    /// Write a value. Source is either `--value-from <file>` or stdin.
    Put {
        /// Namespace (`[a-z0-9-]{1,64}`).
        namespace: String,
        /// 64-char hex key.
        hex_key: String,
        /// Read value bytes from this file.
        #[arg(long, conflicts_with = "value_from_stdin")]
        value_from: Option<String>,
        /// Read value bytes from stdin.
        #[arg(long, conflicts_with = "value_from")]
        value_from_stdin: bool,
    },
    /// Remove an entry. Idempotent — missing keys exit 0.
    Rm {
        /// Namespace.
        namespace: String,
        /// 64-char hex key.
        hex_key: String,
    },
    /// List entries under a namespace, sorted by hex key. One row per entry:
    /// `<hex>  <bytes>`.
    Ls {
        /// Namespace.
        namespace: String,
    },
    /// Drop every entry under a namespace.
    Clear {
        /// Namespace.
        namespace: String,
    },
    /// Print total bytes and per-namespace bytes.
    Stats,
}

/// Fingerprint subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum FpCommands {
    /// Check if files have changed since last success.
    ///
    /// Exit 0 = operation should run (files changed).
    /// Exit 1 = skip (no changes detected).
    Check {
        /// Root directory to scan (default: current directory).
        #[arg(long, default_value = ".")]
        root: String,

        /// File extensions to include (without dot, e.g., "rs", "cpp").
        /// Cannot be used with --include.
        #[arg(long, conflicts_with = "include")]
        ext: Vec<String>,

        /// Glob patterns for files to include (e.g., "**/*.rs").
        /// Cannot be used with --ext.
        #[arg(long, conflicts_with = "ext")]
        include: Vec<String>,

        /// Patterns or directory names to exclude.
        #[arg(long)]
        exclude: Vec<String>,
    },
    /// Mark the previous check as successful.
    #[command(name = "mark-success")]
    MarkSuccess,
    /// Mark the previous check as failed.
    #[command(name = "mark-failure")]
    MarkFailure,
    /// Invalidate the cache (delete all state).
    Invalidate,
}

/// GitHub Actions cache subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum GhaCacheCommands {
    /// Check if GHA cache API is available (env vars set).
    Status,
    /// Save a directory to the GHA cache (tar+gzip, then upload).
    Save {
        /// Cache key (must be unique per content).
        #[arg(long)]
        key: String,
        /// Path to the directory to cache.
        #[arg(long)]
        path: String,
    },
    /// Restore a directory from the GHA cache.
    Restore {
        /// Cache key to look up.
        #[arg(long)]
        key: String,
        /// Path to restore the directory into.
        #[arg(long)]
        path: String,
    },
}

/// Rust artifact plan subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum RustPlanCommands {
    /// Validate a soldr-generated Rust artifact plan.
    Validate {
        /// Path to the protobuf plan file, or a legacy JSON plan.
        #[arg(long)]
        plan: String,
        /// Print a machine-readable JSON summary.
        #[arg(long)]
        json: bool,
        /// Active zccache session ID whose compile-cache stats should be included.
        #[arg(long = "session-id")]
        session_id: Option<String>,
        /// IPC endpoint for session stats lookup.
        #[arg(long)]
        endpoint: Option<String>,
        /// Journal/log path to report in the summary, overriding the plan path.
        #[arg(long)]
        journal: Option<String>,
        /// Local cache directory for bundle path/key reporting.
        #[arg(long = "cache-dir")]
        cache_dir: Option<String>,
    },
    /// Restore Rust target artifacts from a saved plan bundle.
    Restore {
        /// Path to the protobuf plan file, or a legacy JSON plan.
        #[arg(long)]
        plan: String,
        /// Print a machine-readable JSON summary.
        #[arg(long)]
        json: bool,
        /// Active zccache session ID whose compile-cache stats should be included.
        #[arg(long = "session-id")]
        session_id: Option<String>,
        /// IPC endpoint for session stats lookup.
        #[arg(long)]
        endpoint: Option<String>,
        /// Journal/log path to report in the summary, overriding the plan path.
        #[arg(long)]
        journal: Option<String>,
        /// Cache backend to use.
        #[arg(long, default_value = "auto")]
        backend: RustPlanBackendArg,
        /// Local cache directory used for bundle storage.
        #[arg(long = "cache-dir")]
        cache_dir: Option<String>,
    },
    /// Restore Rust target artifacts from base and delta plan bundles.
    RestoreLayered {
        /// Path to the protobuf plan file, or a legacy JSON plan.
        #[arg(long)]
        plan: String,
        /// Print a machine-readable JSON summary.
        #[arg(long)]
        json: bool,
        /// Active zccache session ID whose compile-cache stats should be included.
        #[arg(long = "session-id")]
        session_id: Option<String>,
        /// IPC endpoint for session stats lookup.
        #[arg(long)]
        endpoint: Option<String>,
        /// Journal/log path to report in the summary, overriding the plan path.
        #[arg(long)]
        journal: Option<String>,
        /// Local cache directory containing the base bundle.
        #[arg(long = "base-cache-dir")]
        base_cache_dir: String,
        /// Local cache directory containing the delta bundle.
        #[arg(long = "delta-cache-dir")]
        delta_cache_dir: String,
    },
    /// Save Rust target artifacts selected by a plan.
    Save {
        /// Path to the protobuf plan file, or a legacy JSON plan.
        #[arg(long)]
        plan: String,
        /// Print a machine-readable JSON summary.
        #[arg(long)]
        json: bool,
        /// Active zccache session ID whose compile-cache stats should be included.
        #[arg(long = "session-id")]
        session_id: Option<String>,
        /// IPC endpoint for session stats lookup.
        #[arg(long)]
        endpoint: Option<String>,
        /// Journal/log path to report in the summary, overriding the plan path.
        #[arg(long)]
        journal: Option<String>,
        /// Cache backend to use.
        #[arg(long, default_value = "auto")]
        backend: RustPlanBackendArg,
        /// Local cache directory used for bundle storage.
        #[arg(long = "cache-dir")]
        cache_dir: Option<String>,
    },
    /// Save only Rust target artifacts that differ from a base plan bundle.
    SaveDelta {
        /// Path to the protobuf plan file, or a legacy JSON plan.
        #[arg(long)]
        plan: String,
        /// Print a machine-readable JSON summary.
        #[arg(long)]
        json: bool,
        /// Active zccache session ID whose compile-cache stats should be included.
        #[arg(long = "session-id")]
        session_id: Option<String>,
        /// IPC endpoint for session stats lookup.
        #[arg(long)]
        endpoint: Option<String>,
        /// Journal/log path to report in the summary, overriding the plan path.
        #[arg(long)]
        journal: Option<String>,
        /// Local cache directory containing the base bundle.
        #[arg(long = "base-cache-dir")]
        base_cache_dir: String,
        /// Local cache directory that will receive the delta bundle.
        #[arg(long = "delta-cache-dir")]
        delta_cache_dir: String,
    },
}

/// Rust artifact plan backend selection.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub(crate) enum RustPlanBackendArg {
    Auto,
    Local,
    Gha,
}

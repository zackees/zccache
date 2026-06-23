//! clap definitions: top-level `Cli`, the `Commands` enum, and re-exports
//! of every per-subcommand enum referenced from it.
//!
//! Per-subcommand `clap::Subcommand` enums live in `subcommands.rs` and
//! are re-exported here so the public path
//! `cli::commands::args::<Name>` is unchanged for callers outside this
//! module.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod subcommands;

pub(crate) use subcommands::{
    CacheCommands, CargoRegistryCommands, DaemonCommands, DaemonProfileCommands,
    DefenderExclusionsCommands, FpCommands, GhaCacheCommands, KvCommands, MesonCommands,
    RustPlanBackendArg, RustPlanCommands, SymbolsCommands,
};

/// Environment-variable reference appended to the top-level `--help` output.
/// Surfaced here so settings that are only readable from `std::env` (i.e. not
/// modeled as clap flags) are discoverable without grepping the README.
/// Issue #672 called out `ZCCACHE_PATH_REMAP` as the specific offender; the
/// other entries are the smaller set users actually flip in practice.
const ENV_VARS_HELP: &str = "\
Environment variables:
  ZCCACHE_PATH_REMAP=auto       Inject compiler path-prefix maps so cached
                                artifacts are sharable across sibling clones /
                                worktrees. Opt-in; default is off. See README
                                'Path remapping' for guarantees and pitfalls.
  ZCCACHE_STRICT_PATHS=MODE     Same as --strict-paths (off | consistent | absolute).
  ZCCACHE_CACHE_DIR=PATH        Override the per-user cache root.
";

/// zccache -- fast local compiler cache.
#[derive(Debug, Parser)]
#[command(name = "zccache", version, about, after_help = ENV_VARS_HELP)]
pub(crate) struct Cli {
    /// Clear the entire artifact cache (same as `zccache clear`).
    #[arg(long)]
    pub clear: bool,

    /// Show daemon and cache statistics (same as `zccache status`).
    #[arg(long)]
    pub show_stats: bool,

    /// Validate compiler path flag spelling: off, consistent, or absolute.
    ///
    /// On Windows, header paths reaching the compiler through both a PCH
    /// and a direct `#include` can be spelled with mixed separators
    /// (e.g. `src\fl/stl/cstdio.h`) — clang then sees one physical file
    /// as two paths and `#pragma once` fails to dedupe across the PCH
    /// boundary. Symptom: `error: redefinition of 'X'` with an
    /// "included multiple times" note that quotes the SAME path on both
    /// sides. Set `--strict-paths=consistent` (or
    /// `ZCCACHE_STRICT_PATHS=consistent`) to catch the spelling drift at
    /// the compile-command level before it reaches clang internals. See
    /// #619 for the worked example.
    #[arg(
        long,
        value_name = "MODE",
        num_args = 0..=1,
        default_missing_value = "absolute",
        require_equals = true
    )]
    pub strict_paths: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Commands {
    /// Start the daemon (if not already running).
    Start,
    /// Stop the daemon.
    #[command(visible_alias = "kill")]
    Stop,
    /// Daemon lifecycle and profiling helpers.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommands,
    },
    /// Show daemon and cache status.
    Status {
        /// Print the daemon status as a JSON document to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Analyze a per-session JSONL compile journal and roll it up into a
    /// hit/miss breakdown by output extension, by tool, and by source file.
    /// Reads the file path passed positionally; does not contact the daemon.
    Analyze {
        /// Path to a compile journal JSONL file (the `--journal` output of
        /// `session-start`).
        journal: String,
        /// Print the analysis as a JSON document on stdout.
        #[arg(long)]
        json: bool,
        /// Restrict the rollup to a single session id.
        #[arg(long)]
        session: Option<String>,
        /// Restrict the rollup to a single crate by `--crate-name`.
        #[arg(long = "crate")]
        crate_name: Option<String>,
        /// Restrict the rollup to one outcome class: `hit`, `miss`,
        /// or `non-cacheable` (errors and link records pass through).
        #[arg(long)]
        outcome: Option<String>,
        /// Sort order for the human-readable per-crate table.
        /// One of `wall-clock` (default), `misses`, `hits`.
        #[arg(long, default_value = "wall-clock")]
        sort: String,
        /// Limit the per-crate table to the top N rows.
        #[arg(long)]
        top: Option<usize>,
    },
    /// Clear the artifact cache.
    Clear,
    /// Install the running-process `zccache.servicedef` so the shared
    /// broker can discover and verify the zccache daemon.
    #[command(name = "install-servicedef")]
    InstallServicedef {
        /// Path to the zccache-daemon binary (default: the binary next to
        /// this executable, or `zccache-daemon` found on PATH).
        #[arg(long)]
        daemon_binary: Option<String>,
        /// Install into this directory instead of the default
        /// running-process service-definition directory.
        #[arg(long)]
        dir: Option<String>,
    },
    /// Start a build session. Prints session ID to stdout.
    #[command(name = "session-start")]
    SessionStart {
        /// Working directory (defaults to current dir).
        #[arg(long)]
        cwd: Option<String>,
        /// Path to a log file for this session.
        #[arg(long)]
        log: Option<String>,
        /// IPC endpoint (default: platform-specific).
        #[arg(long)]
        endpoint: Option<String>,
        /// Cache root for the daemon/session (sets ZCCACHE_CACHE_DIR for daemon spawn).
        #[arg(long)]
        cache_dir: Option<String>,
        /// Opt this session into private daemon lifetime semantics.
        #[arg(long)]
        private_daemon: bool,
        /// Portable private daemon name used to derive socket/pipe and lock names.
        #[arg(long)]
        daemon_name: Option<String>,
        /// Owner PID that keeps a private daemon alive. May be repeated.
        #[arg(long)]
        owner_pid: Vec<u32>,
        /// Private session env var as KEY=VALUE. May be repeated; values are redacted in status.
        #[arg(long = "private-env", value_name = "KEY=VALUE")]
        private_env: Vec<String>,
        /// Enable per-session hit/miss statistics tracking.
        #[arg(long)]
        stats: bool,
        /// Write a per-session JSONL compile journal to this path (must end in .jsonl).
        #[arg(long)]
        journal: Option<String>,
        /// Issue #256: opt in to the extended journal schema.
        /// When set, every compile journal line written for this
        /// session also carries `crate_name`, `crate_type`,
        /// `output_ext`, and `self_profile_ns` span timings.
        /// When omitted, behavior is identical to releases before
        /// the flag existed (no new allocations, no extra fields).
        #[arg(long)]
        profile: bool,
    },
    /// End a build session.
    #[command(name = "session-end")]
    SessionEnd {
        /// Session ID to end.
        session_id: String,
        /// IPC endpoint (default: platform-specific).
        #[arg(long)]
        endpoint: Option<String>,
        /// Print final session statistics as JSON to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Query stats for an active session (without ending it).
    #[command(name = "session-stats")]
    SessionStatsCmd {
        /// Session ID to query.
        session_id: String,
        /// IPC endpoint (default: platform-specific).
        #[arg(long)]
        endpoint: Option<String>,
        /// Print active session statistics as JSON to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Wrap a compiler invocation (explicit mode).
    Wrap {
        /// Validate compiler path flag spelling: off, consistent, or absolute.
        ///
        /// On Windows, mixed-separator include paths can defeat clang's
        /// `#pragma once` dedup across PCH/consumer-TU boundaries — see
        /// #619 and the global `--strict-paths` help above.
        #[arg(
            long,
            value_name = "MODE",
            num_args = 0..=1,
            default_missing_value = "absolute",
            require_equals = true
        )]
        strict_paths: Option<String>,
        /// The compiler and its arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Show detailed information about a cache entry.
    Inspect {
        /// Cache key (hex).
        key: String,
    },
    /// Show or clear crash dumps from previous daemon crashes.
    Crashes {
        /// Delete all crash dumps.
        #[arg(long)]
        clear: bool,
    },
    /// Fingerprint-based file change detection.
    ///
    /// Answers "have files changed since the last successful operation?" by
    /// querying the daemon's in-memory watch state (<1ms on cache hit).
    #[command(name = "fp")]
    Fp {
        /// Path to the cache file (e.g., .cache/lint.json).
        #[arg(long)]
        cache_file: String,

        /// Cache algorithm: hash or two-layer.
        #[arg(long, default_value = "two-layer")]
        cache_type: String,

        /// IPC endpoint (default: platform-specific).
        #[arg(long)]
        endpoint: Option<String>,

        #[command(subcommand)]
        fp_command: FpCommands,
    },
    /// Convert an Arduino `.ino` sketch into a generated `.ino.cpp`.
    #[command(name = "ino")]
    Ino {
        /// Input `.ino` file.
        #[arg(long)]
        input: String,
        /// Output `.ino.cpp` file.
        #[arg(long)]
        output: String,
        /// Extra clang arguments used when parsing the `.ino`.
        #[arg(long = "clang-arg")]
        clang_args: Vec<String>,
        /// Do not inject `#include <Arduino.h>`.
        #[arg(long)]
        no_arduino_include: bool,
    },
    /// GitHub Actions cache operations.
    #[command(name = "gha-cache")]
    GhaCache {
        #[command(subcommand)]
        action: GhaCacheCommands,
    },
    /// Execute versioned Rust artifact cache plans.
    #[command(name = "rust-plan")]
    RustPlan {
        #[command(subcommand)]
        action: RustPlanCommands,
    },
    /// Download and optionally unarchive an artifact using the dedicated download daemon.
    Download {
        /// Source URL for a normal single-file download.
        #[arg(long)]
        url: Option<String>,
        /// One explicit URL per multipart segment, in concatenation order.
        #[arg(long = "part-url")]
        part_urls: Vec<String>,
        /// Optional archive/cache path. If omitted, zccache chooses a deterministic cache path.
        archive_path: Option<String>,
        /// Optional destination to expand or unarchive into.
        #[arg(long = "unarchive")]
        unarchive_path: Option<String>,
        /// Optional expected SHA-256 of the downloaded artifact.
        #[arg(long = "sha256")]
        expected_sha256: Option<String>,
        /// Number of parallel range connections to use for single-URL downloads.
        #[arg(long)]
        max_connections: Option<usize>,
        /// Minimum segment size before single-URL downloads switch to ranged fetching.
        #[arg(long)]
        min_segment_size: Option<u64>,
        /// Return immediately with `locked` if another client owns the artifact lock.
        #[arg(long)]
        no_wait: bool,
        /// Report what would happen without mutating the filesystem.
        #[arg(long)]
        dry_run: bool,
        /// Force re-download and re-expand even if cached state is already valid.
        #[arg(long)]
        force: bool,
    },
    /// Manage cargo registry cache (save/restore/hash/clean).
    #[command(name = "cargo-registry")]
    CargoRegistry {
        #[command(subcommand)]
        action: CargoRegistryCommands,
    },
    /// Namespaced blake3-keyed key/value store.
    ///
    /// Backed by `~/.zccache/index.redb` (separate redb table) and spilled
    /// payloads under `~/.zccache/kv/<namespace>/<hex>.bin`. See issue #130.
    Kv {
        #[command(subcommand)]
        action: KvCommands,
    },
    /// Pre-populate target/ with cached artifacts for near-instant builds.
    Warm {
        /// Cargo target directory (default: ./target).
        #[arg(long, default_value = "target")]
        target_dir: String,
        /// Build profile (default: debug).
        #[arg(long, default_value = "debug")]
        profile: String,
        /// IPC endpoint (default: platform-specific).
        #[arg(long)]
        endpoint: Option<String>,
    },
    /// Sum byte size of regular files under a target directory, with optional
    /// pruning. Used by `action/cleanup/prepare-target-snapshot.sh` instead of
    /// Python `os.walk` because jwalk parallelizes readdir+stat across cores —
    /// big win on Windows where per-file Defender callbacks dominate the walk.
    /// Prints total bytes as a decimal integer on stdout. See zccache#189.
    #[command(name = "snapshot-bytes")]
    SnapshotBytes {
        /// Directory to walk.
        #[arg(long)]
        target: PathBuf,
        /// Skip `incremental/` directories during the walk.
        #[arg(long)]
        prune_incremental: bool,
        /// Skip `*/build/*/out/` directories during the walk.
        #[arg(long)]
        prune_build_script_out: bool,
    },
    /// Pre-tar: record blake3 hashes of every workspace source tracked by
    /// each crate's cargo dep-info. The sidecar manifest written under
    /// `<target>/.zccache-fp-manifest.json` lets `snapshot-fp-validate`
    /// (run post-restore on a different runner) decide *per crate* which
    /// fingerprints still match the current source tree. See the rationale
    /// in `snapshot_fp.rs`.
    #[command(name = "snapshot-fp-record")]
    SnapshotFpRecord {
        /// Cargo target directory (default: ./target).
        #[arg(long, default_value = "target")]
        target_dir: PathBuf,
        /// Workspace root (paths in the manifest are stored relative to this).
        /// Defaults to the current working directory.
        #[arg(long)]
        workspace_root: Option<PathBuf>,
        /// Build profile under target/ to walk (default: debug).
        #[arg(long, default_value = "debug")]
        profile: String,
        /// Manifest output path (default: `<target>/.zccache-fp-manifest.json`).
        #[arg(long)]
        manifest_path: Option<PathBuf>,
    },
    /// Post-restore: read the manifest and bump only the dep-info mtimes of
    /// crates whose every tracked source still matches its recorded hash.
    /// Crates with any mismatch are left alone so cargo's normal
    /// `source.mtime > dep_info.mtime → rebuild` check fires for them.
    #[command(name = "snapshot-fp-validate")]
    SnapshotFpValidate {
        #[arg(long, default_value = "target")]
        target_dir: PathBuf,
        #[arg(long)]
        workspace_root: Option<PathBuf>,
        #[arg(long, default_value = "debug")]
        profile: String,
        #[arg(long)]
        manifest_path: Option<PathBuf>,
        /// How far in the future (seconds) to stamp clean crates' dep-info
        /// files. Default 60 matches the existing post-restore touch step
        /// in `action.yml` so output and fingerprint mtimes line up.
        #[arg(long, default_value_t = 60)]
        stamp_seconds_ahead: u64,
    },
    /// Download and install matching debug symbols (PDB/dSYM/dwp) next to
    /// the running zccache binary. See `zccache#276`.
    Symbols {
        #[command(subcommand)]
        action: SymbolsCommands,
    },
    /// Print the resolved cache root directory and exit.
    ///
    /// Reads `ZCCACHE_CACHE_DIR` (or falls back to the platform default) and
    /// prints the absolute path. Wrappers like [soldr](https://github.com/zackees/soldr)
    /// use this to verify at runtime that their cache-redirect env var was
    /// honored by whichever `zccache` binary is on PATH. See issue #275.
    #[command(name = "cache-root")]
    CacheRoot {
        /// Emit `{"cache_root": "<abs>", "source": "<src>"}` instead of the
        /// plain path. `source` is one of `env:ZCCACHE_CACHE_DIR`,
        /// `colocate:cross_volume`, `default:platform_dirs`.
        #[arg(long)]
        json: bool,
    },
    /// Inspect or modify Windows Defender real-time-scan exclusions for the
    /// cache root. No-ops cleanly on non-Windows. See `zccache#273`.
    #[command(name = "defender-exclusions")]
    DefenderExclusions {
        #[command(subcommand)]
        action: DefenderExclusionsCommands,
    },
    /// Issue #272: cache an arbitrary tool's invocation through the daemon.
    ///
    /// Inputs are explicit: declare every file/env var the tool reads so the
    /// cache key reflects them. On a hit the tool is NOT spawned; cached
    /// stdout/stderr/exit-code and `--output-file` paths are replayed.
    ///
    /// Example:
    ///   zccache exec --input-file src/foo.cpp \
    ///                --input-env LINT_VER \
    ///                --output-file report.json \
    ///                -- fastled-lint src/foo.cpp --json
    Exec {
        /// Repeatable: declare a file whose content feeds the cache key.
        #[arg(long = "input-file", value_name = "PATH")]
        input_file: Vec<String>,
        /// Repeatable: env var name whose *value* feeds the cache key.
        /// The current process env is queried for the value at request time.
        #[arg(long = "input-env", value_name = "NAME")]
        input_env: Vec<String>,
        /// Opaque bytes mixed into the cache key (caller-defined namespacing,
        /// e.g. a tool config version).
        #[arg(long = "input-extra", value_name = "BYTES")]
        input_extra: Option<String>,
        /// Capture stdout and include it in the cache. Default: true.
        #[arg(long = "output-stdout", default_value_t = true)]
        output_stdout: bool,
        /// Capture stderr and include it in the cache. Default: true.
        #[arg(long = "output-stderr", default_value_t = true)]
        output_stderr: bool,
        /// Repeatable: file the tool writes; snapshot post-run, restore on hit.
        #[arg(long = "output-file", value_name = "PATH")]
        output_file: Vec<String>,
        /// Caller-supplied tool identity hash (hex). When omitted the daemon
        /// hashes the resolved tool binary (cached by mtime+size).
        #[arg(long = "tool-hash", value_name = "HEX")]
        tool_hash: Option<String>,
        /// Bypass the cache entirely — do not look up, do not store.
        #[arg(long = "no-cache")]
        no_cache: bool,
        /// Do not include the CWD in the cache key. Useful for tools whose
        /// output is path-independent.
        #[arg(long = "no-cwd-in-key")]
        no_cwd_in_key: bool,
        /// IPC endpoint (default: platform-specific).
        #[arg(long)]
        endpoint: Option<String>,
        /// Path A: scan this C/C++-style file for `#include` directives and
        /// mix every transitively-resolved header's content into the key.
        /// Repeatable.
        #[arg(long = "include-scan", value_name = "PATH")]
        include_scan: Vec<String>,
        /// `-I` directory used while resolving `--include-scan`. Repeatable.
        #[arg(long = "include-dir", value_name = "DIR")]
        include_dir: Vec<String>,
        /// `-isystem` directory used while resolving `--include-scan`.
        /// Repeatable.
        #[arg(long = "system-include", value_name = "DIR")]
        system_include: Vec<String>,
        /// `-iquote` directory (quoted-only) used while resolving
        /// `--include-scan`. Repeatable.
        #[arg(long = "iquote-dir", value_name = "DIR")]
        iquote_dir: Vec<String>,
        /// Path B: depfile the tool emits. The daemon parses it on first
        /// run, stores the dep set, and consults it on subsequent runs.
        #[arg(long = "depfile", value_name = "PATH")]
        depfile: Option<String>,
        /// Treat the run as non-deterministic (no caching). Counterpart to
        /// the link handler's `D`/`/DETERMINISTIC` warning.
        #[arg(long = "non-deterministic")]
        non_deterministic: bool,
        /// Regex whose matches are dropped from the cache-key arg list (the
        /// tool still receives them). Repeatable. Useful for runtime-only
        /// flags like `--verbose` or `--no-color` that don't affect output.
        #[arg(long = "key-args-filter", value_name = "REGEX")]
        key_args_filter: Vec<String>,
        /// Everything after `--` is the tool command and its args.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, last = true)]
        tool_command: Vec<String>,
    },
    /// Issue #391: cache `cc` (gcc/clang frontend) invocations. Intended for
    /// `CC="zccache cc"` setups so cc-rs build scripts (libsqlite3-sys etc.)
    /// route through the daemon. Internally forwards to the existing wrap
    /// path with the resolved cc binary on PATH; the Gcc compile parser
    /// handles `-c <input> -o <output>` plus the conservative `-D`/`-I`/`-O`
    /// flag surface.
    ///
    /// Example:
    ///   CC="zccache cc" cargo build -p libsqlite3-sys
    ///   zccache cc -c sqlite3.c -o sqlite3.o
    Cc {
        /// The cc-style argv passed through to the resolved compiler. Use
        /// `--` before the tool args if any leading positional looks like a
        /// clap flag.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Meson build-system caching helpers. Issue #627.
    Meson {
        #[command(subcommand)]
        command: MesonCommands,
    },
    /// Per-version cache directory management (#695).
    ///
    /// Phase 1 surface — today's zccache uses a single cache root and these
    /// commands operate on that root. The same subcommand surface will
    /// extend to the multi-version `~/.zccache/v-<version>/` layout once
    /// the router architecture (#694) lands; until then `cache size` is the
    /// only operation that's meaningful on a single-root cache (`cache list`
    /// and `cache prune` will activate when per-version directories exist).
    Cache {
        #[command(subcommand)]
        command: CacheCommands,
    },
    /// Ask the daemon to release every session handle under a path
    /// (#694 Phase 2, builds on #690).
    ///
    /// Useful before `git worktree remove` / `rmdir` on Windows where the
    /// daemon's per-session journal and log handles would otherwise block
    /// deletion. The daemon refuses to release handles whose target
    /// overlaps the cache root.
    #[command(name = "release-handles")]
    ReleaseHandles {
        /// Path under which all daemon-held session handles should be released.
        #[arg(long)]
        path: PathBuf,
        /// Emit a JSON summary instead of the plain text summary.
        #[arg(long)]
        json: bool,
        /// IPC endpoint (default: platform-specific).
        #[arg(long)]
        endpoint: Option<String>,
    },
}

/// Known subcommand names for auto-detect.
///
/// MUST stay in sync with the `Commands` enum above — any name added to
/// the enum without being listed here gets routed into wrap mode (the
/// `mod.rs` auto-detect path) and surfaces as "daemon error: failed to
/// run compiler: program not found" instead of dispatching normally. The
/// `known_subcommands_matches_clap_enum` test in
/// `cli/commands/tests/args_parsing.rs` enforces this contract.
pub(crate) const KNOWN_SUBCOMMANDS: &[&str] = &[
    "start",
    "stop",
    "daemon",
    "status",
    "analyze",
    "clear",
    "install-servicedef",
    "wrap",
    "inspect",
    "session-start",
    "session-end",
    "session-stats",
    "crashes",
    "fp",
    "ino",
    "download",
    "cargo-registry",
    "gha-cache",
    "rust-plan",
    "kv",
    "meson",
    "warm",
    "snapshot-bytes",
    "snapshot-fp-record",
    "snapshot-fp-validate",
    "symbols",
    "cache",
    "cache-root",
    "release-handles",
    "defender-exclusions",
    "exec",
    "cc",
    "help",
    "--help",
    "-h",
    "--version",
    "-V",
];

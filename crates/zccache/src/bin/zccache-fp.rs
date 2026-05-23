#[cfg(unix)]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(windows)]
#[global_allocator]
static GLOBAL_WIN: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::Path;
use std::process::ExitCode;
use zccache::core::NormalizedPath;

use clap::{Parser, Subcommand, ValueEnum};
use zccache::fingerprint::{
    walk_files, walk_files_glob, FingerprintError, HashCache, TwoLayerCache,
};

/// Fingerprint-based file change detection for CI and tooling.
///
/// Answers "has this set of files changed since the last successful operation?"
/// using blake3 content hashing with mtime fast-paths.
#[derive(Debug, Parser)]
#[command(name = "zccache-fp", version, about)]
struct Cli {
    /// Path to the cache file (e.g., .cache/lint.json).
    #[arg(long)]
    cache_file: NormalizedPath,

    /// Cache algorithm to use.
    #[arg(long, value_enum, default_value_t = CacheType::TwoLayer)]
    cache_type: CacheType,

    #[command(subcommand)]
    command: Commands,
}

/// Cache algorithm.
#[derive(Debug, Clone, ValueEnum)]
enum CacheType {
    /// Single aggregate blake3 hash of entire file set.
    Hash,
    /// Per-file mtime+size → blake3 with smart touch handling.
    TwoLayer,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Check if files have changed since last success.
    ///
    /// Exit 0 = operation should run (files changed).
    /// Exit 1 = skip (no changes detected).
    Check {
        /// Root directory to scan (defaults to current directory).
        #[arg(long, default_value = ".")]
        root: NormalizedPath,

        /// File extensions to include (without dot, repeatable).
        /// Cannot be combined with --include.
        #[arg(long, conflicts_with = "include")]
        ext: Vec<String>,

        /// Glob patterns for files to include (repeatable).
        /// Cannot be combined with --ext.
        #[arg(long, conflicts_with = "ext")]
        include: Vec<String>,

        /// Patterns to exclude (repeatable).
        /// With --ext: directory names (e.g., target).
        /// With --include: glob patterns (e.g., "target/**").
        #[arg(long)]
        exclude: Vec<String>,
    },

    /// Mark the previous check as successful.
    #[command(name = "mark-success")]
    MarkSuccess,

    /// Mark the previous check as failed (forces re-run next time).
    #[command(name = "mark-failure")]
    MarkFailure,

    /// Delete the cache file (forces re-run on next check).
    Invalidate,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode, FingerprintError> {
    match cli.command {
        Commands::Check {
            root,
            ext,
            include,
            exclude,
        } => run_check(
            &cli.cache_file,
            &cli.cache_type,
            &root,
            &ext,
            &include,
            &exclude,
        ),
        Commands::MarkSuccess => run_mark(&cli.cache_file, &cli.cache_type, true),
        Commands::MarkFailure => run_mark(&cli.cache_file, &cli.cache_type, false),
        Commands::Invalidate => run_invalidate(&cli.cache_file, &cli.cache_type),
    }
}

fn run_check(
    cache_file: &Path,
    cache_type: &CacheType,
    root: &Path,
    ext: &[String],
    include: &[String],
    exclude: &[String],
) -> Result<ExitCode, FingerprintError> {
    // Try dir-mtime fast-path first (no file walking needed).
    let fast = match cache_type {
        CacheType::Hash => HashCache::new(cache_file.to_path_buf()).try_skip_fast(root)?,
        CacheType::TwoLayer => TwoLayerCache::new(cache_file.to_path_buf()).try_skip_fast(root)?,
    };
    if let Some(decision) = fast {
        eprintln!("{decision}");
        return Ok(if decision.should_skip() {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        });
    }

    // Full path: walk files and check.
    let files = if !include.is_empty() {
        let inc: Vec<&str> = include.iter().map(String::as_str).collect();
        let exc: Vec<&str> = exclude.iter().map(String::as_str).collect();
        walk_files_glob(root, &inc, &exc)?
    } else {
        let exts: Vec<&str> = ext.iter().map(String::as_str).collect();
        let dirs: Vec<&str> = exclude.iter().map(String::as_str).collect();
        walk_files(root, &exts, &dirs)?
    };

    eprintln!("scanned {} files from {}", files.len(), root.display());

    let decision = match cache_type {
        CacheType::Hash => HashCache::new(cache_file.to_path_buf()).check(&files)?,
        CacheType::TwoLayer => TwoLayerCache::new(cache_file.to_path_buf()).check(&files)?,
    };

    eprintln!("{decision}");

    Ok(if decision.should_skip() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn run_mark(
    cache_file: &Path,
    cache_type: &CacheType,
    success: bool,
) -> Result<ExitCode, FingerprintError> {
    // Auto-detect cache type from pending file so users don't need to pass
    // --cache-type on mark-success/mark-failure (fixes #1 in BUGS.md).
    let cache_type = match zccache::fingerprint::detect_pending_type(cache_file) {
        Some("hash") => CacheType::Hash,
        Some("two-layer") => CacheType::TwoLayer,
        _ => cache_type.clone(),
    };
    match (&cache_type, success) {
        (CacheType::Hash, true) => HashCache::new(cache_file.to_path_buf()).mark_success()?,
        (CacheType::Hash, false) => HashCache::new(cache_file.to_path_buf()).mark_failure()?,
        (CacheType::TwoLayer, true) => {
            TwoLayerCache::new(cache_file.to_path_buf()).mark_success()?
        }
        (CacheType::TwoLayer, false) => {
            TwoLayerCache::new(cache_file.to_path_buf()).mark_failure()?
        }
    }
    let label = if success { "success" } else { "failure" };
    eprintln!("marked {label}");
    Ok(ExitCode::SUCCESS)
}

fn run_invalidate(cache_file: &Path, cache_type: &CacheType) -> Result<ExitCode, FingerprintError> {
    match cache_type {
        CacheType::Hash => HashCache::new(cache_file.to_path_buf()).invalidate()?,
        CacheType::TwoLayer => TwoLayerCache::new(cache_file.to_path_buf()).invalidate()?,
    }
    eprintln!("cache invalidated");
    Ok(ExitCode::SUCCESS)
}

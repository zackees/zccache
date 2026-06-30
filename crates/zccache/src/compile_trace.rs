//! Per-sub-phase JSONL trace inside the embedded compile path.
//!
//! Diagnostic-only — written when the `ZCCACHE_INNER_TRACE` env var
//! points at a writable file path. Off by default (one atomic `OnceLock`
//! load per call, no allocation when disabled). Records elapsed micros
//! per named sub-phase plus optional byte counters, one JSON object per
//! line:
//!
//! ```jsonl
//! {"ts_ns":<u128>, "phase":"<name>", "micros":<u64>, "compile_id":"<str>"}
//! ```
//!
//! ## Why this exists
//!
//! The soldr-side per-phase JSONL trace
//! (`crates/soldr-cli/src/daemon/compile_trace.rs` in zackees/soldr,
//! soldr#985) revealed that **99.7% of cold-build dispatch time on a
//! medium-fixture cargo build sits inside `ZccacheService::compile`**
//! as a single opaque async call. The wire-side IPC framing (stdout
//! chunk loop, stderr chunk loop, terminal frame) totals 0.04% of
//! dispatch budget — under 40 ms across 146 compiles.
//!
//! That number falsified the prior "buffer-elimination" optimization
//! plan in zccache#939 (two attempts moved cold by zero seconds). The
//! actually-load-bearing work — input hashing, cache lookup, the rustc
//! subprocess, pipe drains, the cache-miss store — all sit inside
//! the embedded compile pipeline where soldr can't see them.
//!
//! This module is the diagnostic layer that lets us see them.
//!
//! ## Wire format and ABI compatibility with soldr's trace
//!
//! The JSONL shape is byte-for-byte the same as soldr's daemon trace.
//! soldr's `bench/parse_compile_trace.py` reads either file with no
//! changes — it buckets by the `phase` field. Hosts that point
//! `ZCCACHE_INNER_TRACE` and `SOLDR_DAEMON_TRACE` at *the same path*
//! get a unified per-compile timeline; pointing them at different
//! paths keeps the two layers separate.
//!
//! ## Hot-path cost
//!
//! - **Off** (env var unset): one [`std::sync::OnceLock::get_or_init`] miss
//!   resolves to `None`; subsequent calls hit the `None` arm and
//!   return immediately. Below noise.
//! - **On**: one [`std::time::Instant::elapsed`], one [`format!`], one mutex-guarded
//!   [`std::io::Write::write_all`]. The mutex contention is bounded by the
//!   number of sub-phase records per compile (~6 today) times the
//!   embedded daemon's compile concurrency — fine for diagnostic use,
//!   not enabled in production.
//!
//! Errors during open + write are silently dropped. The trace site
//! **must never** block, fail, or perturb the compile it's measuring.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static TRACE_FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();

/// Name of the env var that, when set to a writable file path, enables
/// the JSONL trace. Unset = trace is off.
pub const ENV_VAR: &str = "ZCCACHE_INNER_TRACE";

fn init() -> Option<Mutex<std::fs::File>> {
    let path = std::env::var_os(ENV_VAR)?;
    let path = PathBuf::from(path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(file) => {
            // One-line startup confirmation so hosts can verify the
            // env var was picked up by THIS process.
            eprintln!("zccache: {ENV_VAR} active, writing to {}", path.display());
            Some(Mutex::new(file))
        }
        Err(e) => {
            eprintln!("zccache: {ENV_VAR}={} but open failed: {e}", path.display());
            None
        }
    }
}

fn writer() -> Option<&'static Mutex<std::fs::File>> {
    TRACE_FILE.get_or_init(init).as_ref()
}

/// Append a single phase-record line to the trace file. No-op when
/// [`ENV_VAR`] is unset. Errors silently dropped — the trace file is
/// diagnostic-only and must never block compile work.
///
/// `phase` is a short stable name (snake_case is conventional);
/// `micros` is the elapsed wall time in microseconds; `compile_id` is
/// the per-compile identifier the caller already tracks for audit
/// correlation (the embedded API surfaces this through
/// [`crate::audit::AuditId`] / `CompileResponse::compile_id`).
pub fn record(phase: &str, micros: u64, compile_id: &str) {
    let Some(file_mu) = writer() else { return };
    let ts_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let line = format!(
        r#"{{"ts_ns":{ts_ns},"phase":"{}","micros":{micros},"compile_id":"{}"}}{nl}"#,
        escape(phase),
        escape(compile_id),
        nl = "\n",
    );
    if let Ok(mut guard) = file_mu.lock() {
        let _ = guard.write_all(line.as_bytes());
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', r"\\").replace('"', r#"\""#)
}

/// RAII guard that records on drop. Use for scope-bounded sub-phases:
///
/// ```ignore
/// {
///     let _p = Phase::start("cache_lookup", &compile_id);
///     // … work …
/// } // recorded here
/// ```
pub struct Phase<'a> {
    name: &'a str,
    compile_id: &'a str,
    start: std::time::Instant,
}

impl<'a> Phase<'a> {
    pub fn start(name: &'a str, compile_id: &'a str) -> Self {
        Self {
            name,
            compile_id,
            start: std::time::Instant::now(),
        }
    }
}

impl Drop for Phase<'_> {
    fn drop(&mut self) {
        let micros = self.start.elapsed().as_micros() as u64;
        record(self.name, micros, self.compile_id);
    }
}

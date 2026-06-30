//! Background writer thread + JSONL rotation/GC.
//!
//! Same lock-free channel + background `std::thread` pattern as
//! `EventLogger`. Serialization happens on the caller's tokio task; this
//! thread does file I/O only. Zero contention on the hot path.
//!
//! Performance notes (ISSUE-101 / ISSUE-301 / ISSUE-302):
//! - Channel is `std::sync::mpsc` (sync receiver) instead of
//!   `tokio::sync::mpsc::UnboundedReceiver` so the background `std::thread`
//!   no longer traverses tokio's parking_lot chain on every wakeup.
//! - The loop drains all currently-queued messages into a batch after each
//!   blocking `recv()`, mirroring the WAL writer drain pattern in
//!   `server/wal.rs`. This coalesces wakeups across burst arrivals (e.g. a
//!   build with many parallel compiles) — drain coalesces wakeups, NOT
//!   messages: every Entry still produces exactly one JSONL line.
//! - File handles are wrapped in `BufWriter` so a typical batch yields one
//!   `write(2)` (and at most one `fdatasync`) instead of one per entry.
//!
//! Together these cut the `zccache-journal` thread's context switches from
//! ~614 / 34 s down to roughly one per batch under load.

use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::mpsc;

use crate::core::NormalizedPath;

use super::super::event_log::open_append;

/// Maximum global journal size before rotation (50 MB).
pub(super) const JOURNAL_MAX_SIZE: u64 = 50 * 1024 * 1024;
/// Maximum number of rotated journal files to keep.
pub(super) const JOURNAL_MAX_FILES: usize = 3;

/// BufWriter capacity for journal file handles (64 KiB).
const JOURNAL_BUF_CAPACITY: usize = 64 * 1024;

/// Maximum messages drained into a single batch after `recv()` returns.
/// Bounds peak memory; the writer still processes everything queued —
/// remaining messages just split into the next batch (each iteration only
/// pays the buffered-write + flush cost, not a wakeup).
const JOURNAL_BATCH_CAP: usize = 64;

/// Message sent to the background journal writer thread.
pub(super) enum JournalMessage {
    /// Write a line to the global journal and optionally to a session journal.
    Entry {
        line: String,
        session_path: Option<NormalizedPath>,
    },
    /// Close a session journal file handle.
    CloseSession { path: NormalizedPath },
}

/// Buffered writer over the global journal file.
type GlobalWriter = BufWriter<std::fs::File>;
/// Buffered writers over each session journal file.
type SessionWriter = BufWriter<std::fs::File>;

/// Background thread: receives journal messages and writes to files.
pub(super) fn journal_thread(
    rx: mpsc::Receiver<JournalMessage>,
    global_path: NormalizedPath,
    global_file: std::fs::File,
) {
    let mut session_files: HashMap<NormalizedPath, SessionWriter> = HashMap::new();
    let mut current_size: u64 = global_path.metadata().map(|m| m.len()).unwrap_or(0);
    let mut global_file: GlobalWriter = BufWriter::with_capacity(JOURNAL_BUF_CAPACITY, global_file);
    // Reused batch buffer so we don't reallocate every wakeup.
    let mut batch: Vec<JournalMessage> = Vec::with_capacity(JOURNAL_BATCH_CAP);

    while let Ok(msg) = rx.recv() {
        batch.push(msg);
        // Drain whatever else is already queued. Bounded by JOURNAL_BATCH_CAP
        // so a runaway producer can't blow up memory in one batch.
        while batch.len() < JOURNAL_BATCH_CAP {
            match rx.try_recv() {
                Ok(more) => batch.push(more),
                Err(_) => break,
            }
        }

        // Track which session writers were touched this batch so we can
        // flush exactly those at the end instead of every cached handle.
        let mut touched_global = false;
        let mut touched_sessions: Vec<NormalizedPath> = Vec::new();

        for msg in batch.drain(..) {
            match msg {
                JournalMessage::Entry { line, session_path } => {
                    // Rotate if over size limit. Flush any buffered data so it
                    // lands in the file we're about to rename, not the new one.
                    if current_size > JOURNAL_MAX_SIZE {
                        let _ = global_file.flush();
                        if let Some((new_file, new_size)) = rotate_journal(&global_path) {
                            global_file = BufWriter::with_capacity(JOURNAL_BUF_CAPACITY, new_file);
                            current_size = new_size;
                        }
                    }

                    // Write to global journal.
                    let line_bytes = line.len() as u64 + 1; // +1 for newline
                    if writeln!(global_file, "{line}").is_err() {
                        if let Ok(f) = open_append(&global_path) {
                            global_file = BufWriter::with_capacity(JOURNAL_BUF_CAPACITY, f);
                            let _ = writeln!(global_file, "{line}");
                        }
                    }
                    current_size += line_bytes;
                    touched_global = true;

                    // Write to session journal if requested.
                    if let Some(path) = session_path {
                        let file = session_files.entry(path.clone()).or_insert_with(|| {
                            match open_append(&path) {
                                Ok(f) => BufWriter::with_capacity(JOURNAL_BUF_CAPACITY, f),
                                Err(e) => {
                                    tracing::debug!("session journal open error: {e}");
                                    // Return a dummy that will fail writes — we'll
                                    // skip silently via is_err() below.
                                    #[expect(
                                        clippy::expect_used,
                                        reason = "NUL on Windows / /dev/null on Unix must exist; their absence indicates a fundamentally broken host where the daemon cannot recover"
                                    )]
                                    let fallback = open_append(&path).unwrap_or_else(|_| {
                                        // Last resort: /dev/null equivalent. The HashMap
                                        // entry will be cleaned up on CloseSession.
                                        std::fs::File::open(if cfg!(windows) {
                                            "NUL"
                                        } else {
                                            "/dev/null"
                                        })
                                        .expect("cannot open null device")
                                    });
                                    BufWriter::with_capacity(JOURNAL_BUF_CAPACITY, fallback)
                                }
                            }
                        });
                        let _ = writeln!(file, "{line}");
                        if !touched_sessions.contains(&path) {
                            touched_sessions.push(path);
                        }
                    }
                }
                JournalMessage::CloseSession { path } => {
                    if let Some(mut writer) = session_files.remove(&path) {
                        let _ = writer.flush();
                    }
                    touched_sessions.retain(|p| p != &path);
                }
            }
        }

        // One flush per batch instead of per message — turns N syscalls into 1.
        if touched_global {
            let _ = global_file.flush();
        }
        for path in &touched_sessions {
            if let Some(writer) = session_files.get_mut(path) {
                let _ = writer.flush();
            }
        }
    }

    // Channel closed: final flush so durable state matches what callers sent.
    let _ = global_file.flush();
    for writer in session_files.values_mut() {
        let _ = writer.flush();
    }
}

/// Rotate the global journal file: rename to timestamped backup, GC old backups.
/// Returns the new file handle and initial size, or `None` on failure.
pub(super) fn rotate_journal(path: &Path) -> Option<(std::fs::File, u64)> {
    let ts =
        super::super::event_log::format_timestamp(std::time::SystemTime::now()).replace(':', "-");
    let rotated = path.with_file_name(format!("compile_journal.jsonl.{ts}"));
    // Rename current file to rotated name.
    if fs::rename(path, &rotated).is_err() {
        return None;
    }
    // Open a fresh file.
    let file = open_append(path).ok()?;
    gc_journal_files(path);
    Some((file, 0))
}

/// Keep only the newest `JOURNAL_MAX_FILES` rotated journal files.
pub(super) fn gc_journal_files(path: &Path) {
    let dir = match path.parent() {
        Some(d) => d,
        None => return,
    };
    let mut rotated: Vec<NormalizedPath> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("compile_journal.jsonl.") {
                Some(e.path().into())
            } else {
                None
            }
        })
        .collect();

    if rotated.len() <= JOURNAL_MAX_FILES {
        return;
    }

    // Sort lexicographically (timestamps sort correctly) — oldest first.
    rotated.sort();
    let to_remove = rotated.len() - JOURNAL_MAX_FILES;
    for p in rotated.into_iter().take(to_remove) {
        let _ = fs::remove_file(p);
    }
}

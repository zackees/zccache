//! Background writer thread + JSONL rotation/GC.
//!
//! Same lock-free channel + background `std::thread` pattern as
//! `EventLogger`. Serialization happens on the caller's tokio task; this
//! thread does file I/O only. Zero contention on the hot path.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use tokio::sync::mpsc;
use zccache::core::NormalizedPath;

use super::super::event_log::open_append;

/// Maximum global journal size before rotation (50 MB).
pub(super) const JOURNAL_MAX_SIZE: u64 = 50 * 1024 * 1024;
/// Maximum number of rotated journal files to keep.
pub(super) const JOURNAL_MAX_FILES: usize = 3;

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

/// Background thread: receives journal messages and writes to files.
pub(super) fn journal_thread(
    mut rx: mpsc::UnboundedReceiver<JournalMessage>,
    global_path: NormalizedPath,
    mut global_file: std::fs::File,
) {
    let mut session_files: HashMap<NormalizedPath, std::fs::File> = HashMap::new();
    let mut current_size: u64 = global_path.metadata().map(|m| m.len()).unwrap_or(0);

    while let Some(msg) = rx.blocking_recv() {
        match msg {
            JournalMessage::Entry { line, session_path } => {
                // Rotate if over size limit.
                if current_size > JOURNAL_MAX_SIZE {
                    if let Some((new_file, new_size)) = rotate_journal(&global_path) {
                        global_file = new_file;
                        current_size = new_size;
                    }
                }

                // Write to global journal.
                let line_bytes = line.len() as u64 + 1; // +1 for newline
                if writeln!(global_file, "{line}").is_err() {
                    if let Ok(f) = open_append(&global_path) {
                        global_file = f;
                        let _ = writeln!(global_file, "{line}");
                    }
                }
                current_size += line_bytes;
                // Write to session journal if requested.
                if let Some(ref path) = session_path {
                    let file = session_files.entry(path.clone()).or_insert_with(|| {
                        match open_append(path) {
                            Ok(f) => f,
                            Err(e) => {
                                tracing::debug!("session journal open error: {e}");
                                // Return a dummy that will fail writes — we'll
                                // skip silently via is_err() below.
                                open_append(path).unwrap_or_else(|_| {
                                    // Last resort: /dev/null equivalent. The HashMap
                                    // entry will be cleaned up on CloseSession.
                                    std::fs::File::open(if cfg!(windows) {
                                        "NUL"
                                    } else {
                                        "/dev/null"
                                    })
                                    .expect("cannot open null device")
                                })
                            }
                        }
                    });
                    let _ = writeln!(file, "{line}");
                }
            }
            JournalMessage::CloseSession { path } => {
                session_files.remove(&path);
            }
        }
    }
}

/// Rotate the global journal file: rename to timestamped backup, GC old backups.
/// Returns the new file handle and initial size, or `None` on failure.
pub(super) fn rotate_journal(path: &Path) -> Option<(std::fs::File, u64)> {
    let ts = super::super::event_log::format_timestamp(std::time::SystemTime::now()).replace(':', "-");
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

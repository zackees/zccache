//! End-to-end `CompileJournal` tests: file writes, per-session journals,
//! close/reopen, concurrent logging, rotation, and GC.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::super::journal_thread::{gc_journal_files, rotate_journal, JOURNAL_MAX_FILES};
use super::super::{miss_reason, CompileJournal, JournalContext, JournalEntry};
use super::wait_for_lines;

// ─── Basic file writes ────────────────────────────────────────────────────

#[test]
fn test_journal_file_write() {
    let dir = tempfile::tempdir().unwrap();
    let journal = CompileJournal::new(dir.path().to_path_buf().into());

    let ctx = JournalContext {
        compiler: "/usr/bin/clang++".to_string(),
        args: vec!["-c".to_string(), "test.cpp".to_string()],
        cwd: "/project".to_string(),
        env: None,
        session_id: Some("session-1".to_string()),
    };
    let entry = JournalEntry::new(ctx, "hit", 0, 5_000_000, None);
    journal.log(&entry, None);

    // Give the background thread time to write.
    std::thread::sleep(Duration::from_millis(200));

    let content = fs::read_to_string(dir.path().join("compile_journal.jsonl")).unwrap();
    assert!(!content.is_empty(), "journal should have content");
    // Each line should be valid JSON.
    for line in content.lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["outcome"], "hit");
        assert_eq!(v["compiler"], "/usr/bin/clang++");
    }
}

#[test]
fn test_noop_journal() {
    let journal = CompileJournal::noop();
    let ctx = JournalContext {
        compiler: "clang".to_string(),
        args: vec![],
        cwd: "/tmp".to_string(),
        env: None,
        session_id: None,
    };
    let entry = JournalEntry::new(ctx, "miss", 0, 0, Some(miss_reason::UNKNOWN));
    // Should not panic.
    journal.log(&entry, None);
}

// ─── Session journal writes ───────────────────────────────────────────────

#[test]
fn test_session_journal_file_write() {
    let dir = tempfile::tempdir().unwrap();
    let session_dir = dir.path().join("sessions");
    fs::create_dir_all(&session_dir).unwrap();
    let session_path = session_dir.join("test-session.jsonl");

    let journal = CompileJournal::new(dir.path().to_path_buf().into());

    let ctx = JournalContext {
        compiler: "/usr/bin/clang++".to_string(),
        args: vec!["-c".to_string(), "test.cpp".to_string()],
        cwd: "/project".to_string(),
        env: None,
        session_id: Some("test-session".to_string()),
    };
    let entry = JournalEntry::new(ctx, "miss", 0, 2_000_000, Some(miss_reason::UNKNOWN));
    journal.log(&entry, Some(&session_path));

    // Give the background thread time to write.
    std::thread::sleep(Duration::from_millis(200));

    // Global journal should have the entry.
    let global = fs::read_to_string(dir.path().join("compile_journal.jsonl")).unwrap();
    assert!(!global.is_empty(), "global journal should have content");

    // Session journal should also have the entry.
    let session = fs::read_to_string(&session_path).unwrap();
    assert!(!session.is_empty(), "session journal should have content");
    let v: serde_json::Value = serde_json::from_str(session.trim()).unwrap();
    assert_eq!(v["outcome"], "miss");
    assert_eq!(v["session_id"], "test-session");
}

#[test]
fn test_close_session_releases_handle() {
    let dir = tempfile::tempdir().unwrap();
    let session_dir = dir.path().join("sessions");
    fs::create_dir_all(&session_dir).unwrap();
    let session_path = session_dir.join("close-test.jsonl");

    let journal = CompileJournal::new(dir.path().to_path_buf().into());

    let ctx = JournalContext {
        compiler: "clang".to_string(),
        args: vec![],
        cwd: "/tmp".to_string(),
        env: None,
        session_id: Some("close-test".to_string()),
    };
    let entry = JournalEntry::new(ctx, "hit", 0, 100, None);
    journal.log(&entry, Some(&session_path));
    journal.close_session(&session_path);

    std::thread::sleep(Duration::from_millis(200));

    // File should exist and have content.
    let content = fs::read_to_string(&session_path).unwrap();
    assert!(!content.is_empty());
}

#[test]
fn test_session_multiple_entries_same_path() {
    let dir = tempfile::tempdir().unwrap();
    let session_dir = dir.path().join("sessions");
    fs::create_dir_all(&session_dir).unwrap();
    let session_path = session_dir.join("multi-entry.jsonl");

    let journal = CompileJournal::new(dir.path().to_path_buf().into());

    for i in 0..5 {
        let ctx = JournalContext {
            compiler: format!("clang-{i}"),
            args: vec![],
            cwd: "/tmp".to_string(),
            env: None,
            session_id: Some("multi".to_string()),
        };
        let entry = JournalEntry::new(ctx, "miss", 0, i as u128, Some(miss_reason::UNKNOWN));
        journal.log(&entry, Some(&session_path));
    }

    wait_for_lines(&session_path, 5);

    let content = fs::read_to_string(&session_path).unwrap();
    assert_eq!(content.lines().count(), 5, "session should have 5 entries");
}

#[test]
fn test_multiple_sessions_correct_routing() {
    let dir = tempfile::tempdir().unwrap();
    let session_dir = dir.path().join("sessions");
    fs::create_dir_all(&session_dir).unwrap();
    let path_a = session_dir.join("session-a.jsonl");
    let path_b = session_dir.join("session-b.jsonl");

    let journal = CompileJournal::new(dir.path().to_path_buf().into());

    // Interleave entries between two sessions
    for i in 0..6 {
        let (sid, path) = if i % 2 == 0 {
            ("session-a", path_a.as_path())
        } else {
            ("session-b", path_b.as_path())
        };
        let ctx = JournalContext {
            compiler: "clang".to_string(),
            args: vec![],
            cwd: "/tmp".to_string(),
            env: None,
            session_id: Some(sid.to_string()),
        };
        let entry = JournalEntry::new(ctx, "hit", 0, 0, None);
        journal.log(&entry, Some(path));
    }

    wait_for_lines(&path_a, 3);
    wait_for_lines(&path_b, 3);

    let content_a = fs::read_to_string(&path_a).unwrap();
    let content_b = fs::read_to_string(&path_b).unwrap();

    assert_eq!(
        content_a.lines().count(),
        3,
        "session-a should have 3 entries"
    );
    assert_eq!(
        content_b.lines().count(),
        3,
        "session-b should have 3 entries"
    );

    // Verify routing by session_id
    for line in content_a.lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["session_id"], "session-a");
    }
    for line in content_b.lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["session_id"], "session-b");
    }
}

#[test]
fn test_close_session_then_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let session_dir = dir.path().join("sessions");
    fs::create_dir_all(&session_dir).unwrap();
    let session_path = session_dir.join("reopen.jsonl");

    let journal = CompileJournal::new(dir.path().to_path_buf().into());

    // Write first entry
    let ctx1 = JournalContext {
        compiler: "clang".to_string(),
        args: vec![],
        cwd: "/tmp".to_string(),
        env: None,
        session_id: Some("reopen".to_string()),
    };
    let entry1 = JournalEntry::new(ctx1, "miss", 0, 100, Some(miss_reason::UNKNOWN));
    journal.log(&entry1, Some(&session_path));

    // Close session — releases file handle
    journal.close_session(&session_path);

    // Write second entry — should re-open the file via or_insert_with
    let ctx2 = JournalContext {
        compiler: "clang".to_string(),
        args: vec![],
        cwd: "/tmp".to_string(),
        env: None,
        session_id: Some("reopen".to_string()),
    };
    let entry2 = JournalEntry::new(ctx2, "hit", 0, 200, None);
    journal.log(&entry2, Some(&session_path));

    wait_for_lines(&session_path, 2);

    let content = fs::read_to_string(&session_path).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 2, "should have 2 entries after close+reopen");

    let v0: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    let v1: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(v0["outcome"], "miss");
    assert_eq!(v1["outcome"], "hit");
}

// ─── Noop edge cases ──────────────────────────────────────────────────────

#[test]
fn test_noop_close_session() {
    let journal = CompileJournal::noop();
    // Must not panic
    journal.close_session(Path::new("/nonexistent/session.jsonl"));
}

#[test]
fn test_double_close_session() {
    let dir = tempfile::tempdir().unwrap();
    let session_dir = dir.path().join("sessions");
    fs::create_dir_all(&session_dir).unwrap();
    let session_path = session_dir.join("double-close.jsonl");

    let journal = CompileJournal::new(dir.path().to_path_buf().into());

    let ctx = JournalContext {
        compiler: "clang".to_string(),
        args: vec![],
        cwd: "/tmp".to_string(),
        env: None,
        session_id: Some("dc".to_string()),
    };
    let entry = JournalEntry::new(ctx, "hit", 0, 0, None);
    journal.log(&entry, Some(&session_path));

    // Close twice — second close removes from empty map, must not panic
    journal.close_session(&session_path);
    journal.close_session(&session_path);

    std::thread::sleep(Duration::from_millis(200));

    let content = fs::read_to_string(&session_path).unwrap();
    assert_eq!(content.lines().count(), 1);
}

#[test]
fn test_noop_log_with_session_path() {
    // Noop journal with session_path must not panic or create files
    let journal = CompileJournal::noop();
    let ctx = JournalContext {
        compiler: "clang".to_string(),
        args: vec![],
        cwd: "/tmp".to_string(),
        env: None,
        session_id: Some("x".to_string()),
    };
    let entry = JournalEntry::new(ctx, "miss", 0, 0, Some(miss_reason::UNKNOWN));
    journal.log(&entry, Some(Path::new("/nonexistent/path.jsonl")));
}

// ─── JSONL integrity / concurrency ────────────────────────────────────────

#[test]
fn test_multiple_entries_valid_jsonl() {
    let dir = tempfile::tempdir().unwrap();
    let journal = CompileJournal::new(dir.path().to_path_buf().into());

    for i in 0..50 {
        let ctx = JournalContext {
            compiler: format!("clang-{i}"),
            args: vec![format!("file{i}.c")],
            cwd: "/build".to_string(),
            env: None,
            session_id: None,
        };
        let entry = JournalEntry::new(ctx, "miss", 0, i as u128 * 1000, Some(miss_reason::UNKNOWN));
        journal.log(&entry, None);
    }

    std::thread::sleep(Duration::from_millis(500));

    let content = fs::read_to_string(dir.path().join("compile_journal.jsonl")).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 50, "expected 50 lines, got {}", lines.len());
    for (i, line) in lines.iter().enumerate() {
        let v: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("line {i} invalid JSON: {e}"));
        assert_eq!(v["outcome"], "miss");
    }
}

#[test]
fn test_concurrent_logging() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(CompileJournal::new(dir.path().to_path_buf().into()));

    let mut handles = vec![];
    for t in 0..10 {
        let j = Arc::clone(&journal);
        handles.push(std::thread::spawn(move || {
            for i in 0..100 {
                let ctx = JournalContext {
                    compiler: format!("clang-t{t}"),
                    args: vec![format!("file{i}.c")],
                    cwd: "/build".to_string(),
                    env: None,
                    session_id: Some(format!("thread-{t}")),
                };
                let entry = JournalEntry::new(ctx, "hit", 0, i as u128, None);
                j.log(&entry, None);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    std::thread::sleep(Duration::from_millis(500));

    let content = fs::read_to_string(dir.path().join("compile_journal.jsonl")).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(
        lines.len(),
        1000,
        "expected 1000 lines, got {}",
        lines.len()
    );
    for (i, line) in lines.iter().enumerate() {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|e| panic!("line {i} invalid JSON: {e}"));
    }
}

// ─── Rotation / GC ────────────────────────────────────────────────────────

#[test]
fn test_journal_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("compile_journal.jsonl");

    // Create a journal file that exceeds JOURNAL_MAX_SIZE equivalent
    // by directly calling rotate_journal.
    fs::write(&path, vec![b'x'; 100]).unwrap();
    let result = rotate_journal(&path);
    assert!(result.is_some());

    // Original path should exist (fresh file).
    assert!(path.exists());

    // A rotated file should exist.
    let rotated: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("compile_journal.jsonl.")
        })
        .collect();
    assert_eq!(rotated.len(), 1);
}

#[test]
fn test_journal_gc_keeps_max_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("compile_journal.jsonl");
    fs::write(&path, b"current").unwrap();

    // Create 5 rotated files (more than JOURNAL_MAX_FILES=3).
    for i in 0..5 {
        let rotated = dir.path().join(format!(
            "compile_journal.jsonl.2026-03-{i:02}T00-00-00.000Z"
        ));
        fs::write(&rotated, format!("data-{i}")).unwrap();
    }

    gc_journal_files(&path);

    let remaining: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("compile_journal.jsonl.")
        })
        .collect();
    assert!(
        remaining.len() <= JOURNAL_MAX_FILES,
        "expected at most {JOURNAL_MAX_FILES} rotated files, got {}",
        remaining.len()
    );
}

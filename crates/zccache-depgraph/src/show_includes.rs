//! MSVC `/showIncludes` stderr parser.
//!
//! When MSVC is invoked with `/showIncludes`, it emits one line per included
//! file to stderr.  The prefix is locale-dependent:
//!
//! - English:  `Note: including file:`
//! - Japanese: `ãƒ¡ãƒ¢: ã‚¤ãƒ³ã‚¯ãƒ«ãƒ¼ãƒ‰ ãƒ•ã‚¡ã‚¤ãƒ«:`
//! - Chinese:  `æ³¨æ„: åŒ…å«æ–‡ä»¶:`
//! - German:   `Hinweis: Einlesen der Datei:`
//!
//! We auto-detect the prefix from the output rather than hardcoding a single
//! locale, producing a [`ScanResult`] with `has_computed = false`.

use std::collections::HashSet;
use std::path::Path;

use zccache_core::NormalizedPath;

use super::depfile::canonicalize_path;
use super::scanner::ScanResult;

/// Well-known English prefix â€” checked first as a fast path.
const ENGLISH_PREFIX: &str = "Note: including file:";

/// Maximum partial line retained while waiting for a newline. Include paths
/// are line-oriented and comfortably below this size; an oversized diagnostic
/// is passed through incrementally so one malformed line cannot defeat the
/// streaming compile path's memory bound.
const MAX_PARTIAL_LINE_BYTES: usize = 64 * 1024;

/// Incremental `/showIncludes` parser used by the live compiler-output path.
///
/// Complete lines are classified as chunks arrive. Non-include stderr is
/// appended to the caller-provided output buffer with its original line
/// endings, while include paths are accumulated for [`ScanResult`].
pub struct ShowIncludesParser {
    source_canonical: NormalizedPath,
    cwd: NormalizedPath,
    prefix: Option<String>,
    seen: HashSet<NormalizedPath>,
    resolved: Vec<NormalizedPath>,
    partial: Vec<u8>,
    passthrough_line: bool,
}

impl ShowIncludesParser {
    pub fn new(source: &Path, cwd: &Path) -> Self {
        Self {
            source_canonical: canonicalize_path(source, cwd),
            cwd: NormalizedPath::new(cwd),
            prefix: None,
            seen: HashSet::new(),
            resolved: Vec::new(),
            partial: Vec::new(),
            passthrough_line: false,
        }
    }

    /// Consume another stderr chunk and append filtered bytes to `output`.
    pub fn push(&mut self, mut chunk: &[u8], output: &mut Vec<u8>) {
        while !chunk.is_empty() {
            if self.passthrough_line {
                if let Some(newline) = chunk.iter().position(|byte| *byte == b'\n') {
                    output.extend_from_slice(&chunk[..=newline]);
                    chunk = &chunk[newline + 1..];
                    self.passthrough_line = false;
                } else {
                    output.extend_from_slice(chunk);
                    return;
                }
                continue;
            }

            if let Some(newline) = chunk.iter().position(|byte| *byte == b'\n') {
                self.partial.extend_from_slice(&chunk[..=newline]);
                chunk = &chunk[newline + 1..];
                self.finish_line(output);
            } else {
                self.partial.extend_from_slice(chunk);
                if self.partial.len() > MAX_PARTIAL_LINE_BYTES {
                    output.extend_from_slice(&self.partial);
                    self.partial.clear();
                    self.passthrough_line = true;
                }
                return;
            }
        }
    }

    /// Flush a final unterminated line and return the dependency scan.
    pub fn finish(mut self, output: &mut Vec<u8>) -> ScanResult {
        if !self.partial.is_empty() {
            self.finish_line(output);
        }
        ScanResult {
            resolved: self.resolved,
            unresolved: Vec::new(),
            has_computed: false,
        }
    }

    fn finish_line(&mut self, output: &mut Vec<u8>) {
        let raw = std::mem::take(&mut self.partial);
        let text_end = raw
            .strip_suffix(b"\n")
            .unwrap_or(&raw)
            .strip_suffix(b"\r")
            .unwrap_or_else(|| raw.strip_suffix(b"\n").unwrap_or(&raw));
        let line = String::from_utf8_lossy(text_end);

        if self.prefix.is_none() {
            self.prefix = if line.starts_with(ENGLISH_PREFIX) {
                Some(ENGLISH_PREFIX.to_string())
            } else {
                extract_prefix_candidate(&line)
            };
        }

        let Some(prefix) = self.prefix.as_deref() else {
            output.extend_from_slice(&raw);
            return;
        };
        let Some(path_str) = line.strip_prefix(prefix) else {
            output.extend_from_slice(&raw);
            return;
        };
        let path_str = path_str.trim();
        if path_str.is_empty() {
            return;
        }
        let dep_path = Path::new(path_str);
        let abs_path = if dep_path.is_absolute() {
            canonicalize_path(dep_path, self.cwd.as_path())
        } else {
            let path = self.cwd.join(dep_path);
            canonicalize_path(path.as_path(), self.cwd.as_path())
        };
        if abs_path != self.source_canonical && self.seen.insert(abs_path.clone()) {
            self.resolved.push(abs_path);
        }
    }
}

/// Parse MSVC `/showIncludes` stderr output into a [`ScanResult`].
///
/// Auto-detects the locale-specific prefix, extracts include paths,
/// deduplicates, and returns `has_computed = false` (because the compiler
/// has already resolved all macros).
///
/// Returns `(scan_result, filtered_stderr)` where `filtered_stderr` has
/// `/showIncludes` lines removed, with original line endings and empty
/// lines preserved.
pub fn parse_show_includes(stderr: &[u8], source: &Path, cwd: &Path) -> (ScanResult, Vec<u8>) {
    let source_canonical = canonicalize_path(source, cwd);
    let lines = split_lines_preserving(stderr);

    // Auto-detect the locale-specific prefix (English fast path first).
    let prefix = detect_prefix(&lines);

    let mut seen = HashSet::new();
    let mut resolved = Vec::new();
    let mut filtered = Vec::new();

    for (text, raw) in &lines {
        let mut is_include_line = false;

        if let Some(ref pfx) = prefix {
            let line_str = String::from_utf8_lossy(text);
            if let Some(path_str) = line_str.strip_prefix(pfx.as_str()) {
                let path_str = path_str.trim();
                if !path_str.is_empty() {
                    let dep_path = Path::new(path_str);
                    let abs_path = if dep_path.is_absolute() {
                        canonicalize_path(dep_path, cwd)
                    } else {
                        canonicalize_path(&cwd.join(dep_path), cwd)
                    };

                    if abs_path != source_canonical && seen.insert(abs_path.clone()) {
                        resolved.push(abs_path);
                    }
                }
                is_include_line = true;
            }
        }

        if !is_include_line {
            filtered.extend_from_slice(raw);
        }
    }

    let scan = ScanResult {
        resolved,
        unresolved: Vec::new(),
        has_computed: false,
    };
    (scan, filtered)
}

// â”€â”€ Prefix detection â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Detect the `/showIncludes` prefix from stderr output.
///
/// First checks for the English prefix (fast path), then falls back to
/// auto-detection for non-English MSVC locales by scanning for lines
/// with the pattern `<text>:<whitespace><drive_letter>:\<path>`.
fn detect_prefix(lines: &[(&[u8], &[u8])]) -> Option<String> {
    // Fast path: check for the well-known English prefix.
    for (text, _) in lines {
        let line = String::from_utf8_lossy(text);
        if line.starts_with(ENGLISH_PREFIX) {
            return Some(ENGLISH_PREFIX.to_string());
        }
    }

    // Slow path: auto-detect from drive-letter path patterns.
    // Count candidate prefixes; the most frequent one wins.
    let mut counts: Vec<(String, usize)> = Vec::new();
    for (text, _) in lines {
        let line = String::from_utf8_lossy(text);
        if let Some(candidate) = extract_prefix_candidate(&line) {
            if let Some(entry) = counts.iter_mut().find(|(pfx, _)| *pfx == candidate) {
                entry.1 += 1;
            } else {
                counts.push((candidate, 1));
            }
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(pfx, _)| pfx)
}

/// Extract a candidate `/showIncludes` prefix from a single line.
///
/// Matches: `<text ending with ':'><whitespace><drive_letter>:\<path>`.
/// Returns everything up to and including the colon before the path.
fn extract_prefix_candidate(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    if bytes.len() < 4 {
        return None;
    }

    for i in 0..bytes.len().saturating_sub(1) {
        if !looks_like_windows_path_start(bytes, i) {
            continue;
        }
        // Drive path at start of line is an error/warning location, not /showIncludes.
        if i == 0 {
            continue;
        }

        let before = &line[..i];
        let trimmed = before.trim_end();
        // Prefix must end with ':' and have some text before it.
        if trimmed.len() >= 2 && trimmed.ends_with(':') {
            return Some(trimmed.to_string());
        }
    }
    None
}

fn looks_like_windows_path_start(bytes: &[u8], i: usize) -> bool {
    let len = bytes.len();

    // X:\path
    if i + 2 < len
        && bytes[i].is_ascii_alphabetic()
        && bytes[i + 1] == b':'
        && bytes[i + 2] == b'\\'
    {
        return true;
    }

    // \\server\share or \\?\C:\...
    i + 1 < len && bytes[i] == b'\\' && bytes[i + 1] == b'\\'
}

// â”€â”€ Line splitting â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Split bytes into `(text, raw)` pairs where `text` has line terminators
/// stripped and `raw` preserves the original bytes including terminators.
fn split_lines_preserving(data: &[u8]) -> Vec<(&[u8], &[u8])> {
    let mut lines = Vec::new();
    let mut start = 0;
    for i in 0..data.len() {
        if data[i] == b'\n' {
            let text_end = if i > start && data[i - 1] == b'\r' {
                i - 1
            } else {
                i
            };
            lines.push((&data[start..text_end], &data[start..=i]));
            start = i + 1;
        }
    }
    // Trailing content without newline.
    if start < data.len() {
        lines.push((&data[start..], &data[start..]));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::super::depfile::strip_win_prefix;
    use super::*;

    #[test]
    fn parse_basic() {
        let dir = tempfile::TempDir::new().unwrap();
        let cwd = dir.path();

        // Create real files so canonicalize works.
        let h1 = cwd.join("stdio.h");
        let h2 = cwd.join("stddef.h");
        let source = cwd.join("main.cpp");
        std::fs::write(&h1, "").unwrap();
        std::fs::write(&h2, "").unwrap();
        std::fs::write(&source, "").unwrap();

        let stderr = format!(
            "Note: including file: {}\r\nNote: including file: {}\r\n",
            h1.display(),
            h2.display(),
        );

        let (scan, filtered) = parse_show_includes(stderr.as_bytes(), &source, cwd);

        assert!(!scan.has_computed);
        assert_eq!(scan.resolved.len(), 2);
        let canon_h1 = strip_win_prefix(std::fs::canonicalize(&h1).unwrap().into());
        let canon_h2 = strip_win_prefix(std::fs::canonicalize(&h2).unwrap().into());
        assert!(scan.resolved.contains(&canon_h1));
        assert!(scan.resolved.contains(&canon_h2));
        assert!(filtered.is_empty());
    }

    #[test]
    fn parse_empty_stderr() {
        let (scan, filtered) = parse_show_includes(b"", Path::new("main.cpp"), Path::new("."));
        assert!(!scan.has_computed);
        assert!(scan.resolved.is_empty());
        assert!(scan.unresolved.is_empty());
        assert!(filtered.is_empty());
    }

    #[test]
    fn parse_mixed_output() {
        let dir = tempfile::TempDir::new().unwrap();
        let cwd = dir.path();

        let h1 = cwd.join("foo.h");
        let h2 = cwd.join("bar.h");
        let source = cwd.join("main.cpp");
        std::fs::write(&h1, "").unwrap();
        std::fs::write(&h2, "").unwrap();
        std::fs::write(&source, "").unwrap();

        let stderr = format!(
            "Note: including file: {}\r\nwarning C4996: deprecated\r\nNote: including file: {}\r\n",
            h1.display(),
            h2.display(),
        );

        let (scan, filtered) = parse_show_includes(stderr.as_bytes(), &source, cwd);

        assert_eq!(scan.resolved.len(), 2);
        let filtered_str = String::from_utf8(filtered).unwrap();
        assert!(filtered_str.contains("warning C4996"));
        assert!(!filtered_str.contains("including file"));
    }

    #[test]
    fn incremental_parser_handles_lines_split_across_chunks() {
        let dir = tempfile::TempDir::new().unwrap();
        let cwd = dir.path();
        let header = cwd.join("split.h");
        let source = cwd.join("main.cpp");
        std::fs::write(&header, "").unwrap();
        std::fs::write(&source, "").unwrap();

        let stderr = format!(
            "Note: including file: {}\r\nwarning C4996: split warning\r\n",
            header.display()
        );
        let mut parser = ShowIncludesParser::new(&source, cwd);
        let mut filtered = Vec::new();
        for chunk in stderr.as_bytes().chunks(3) {
            parser.push(chunk, &mut filtered);
        }
        let scan = parser.finish(&mut filtered);

        assert_eq!(scan.resolved.len(), 1);
        assert_eq!(filtered, b"warning C4996: split warning\r\n");
    }

    #[test]
    fn incremental_parser_bounds_oversized_unterminated_line() {
        let mut parser = ShowIncludesParser::new(Path::new("main.cpp"), Path::new("."));
        let diagnostic = vec![b'x'; MAX_PARTIAL_LINE_BYTES * 2];
        let mut filtered = Vec::new();

        for chunk in diagnostic.chunks(4096) {
            parser.push(chunk, &mut filtered);
            assert!(parser.partial.len() <= MAX_PARTIAL_LINE_BYTES);
        }
        let scan = parser.finish(&mut filtered);

        assert!(scan.resolved.is_empty());
        assert_eq!(filtered, diagnostic);
    }

    #[test]
    fn detect_prefix_accepts_unc_paths() {
        if !crate::platform::host::is_windows() {
            return;
        }
        let line = "Hinweis: Einlesen der Datei: \\\\server\\share\\sdk\\foo.h";
        assert_eq!(
            extract_prefix_candidate(line),
            Some("Hinweis: Einlesen der Datei:".to_string())
        );
    }

    #[test]
    fn parse_deduplicates() {
        let dir = tempfile::TempDir::new().unwrap();
        let cwd = dir.path();

        let h1 = cwd.join("dup.h");
        let source = cwd.join("main.cpp");
        std::fs::write(&h1, "").unwrap();
        std::fs::write(&source, "").unwrap();

        let stderr = format!(
            "Note: including file: {}\r\nNote: including file: {}\r\n",
            h1.display(),
            h1.display(),
        );

        let (scan, _) = parse_show_includes(stderr.as_bytes(), &source, cwd);
        assert_eq!(scan.resolved.len(), 1);
    }

    #[test]
    fn parse_excludes_source() {
        let dir = tempfile::TempDir::new().unwrap();
        let cwd = dir.path();

        let source = cwd.join("main.cpp");
        std::fs::write(&source, "").unwrap();

        let stderr = format!("Note: including file: {}\r\n", source.display());

        let (scan, _) = parse_show_includes(stderr.as_bytes(), &source, cwd);
        assert!(scan.resolved.is_empty());
    }

    #[test]
    fn parse_paths_with_spaces() {
        let dir = tempfile::TempDir::new().unwrap();
        let cwd = dir.path();

        let subdir = cwd.join("my headers");
        std::fs::create_dir_all(&subdir).unwrap();
        let h1 = subdir.join("spaced header.h");
        let source = cwd.join("main.cpp");
        std::fs::write(&h1, "").unwrap();
        std::fs::write(&source, "").unwrap();

        let stderr = format!("Note: including file: {}\r\n", h1.display());

        let (scan, _) = parse_show_includes(stderr.as_bytes(), &source, cwd);
        assert_eq!(scan.resolved.len(), 1);
    }

    #[test]
    fn parse_lf_line_endings() {
        let dir = tempfile::TempDir::new().unwrap();
        let cwd = dir.path();

        let h1 = cwd.join("unix.h");
        let source = cwd.join("main.cpp");
        std::fs::write(&h1, "").unwrap();
        std::fs::write(&source, "").unwrap();

        // LF only, no CR
        let stderr = format!("Note: including file: {}\n", h1.display());

        let (scan, _) = parse_show_includes(stderr.as_bytes(), &source, cwd);
        assert_eq!(scan.resolved.len(), 1);
    }

    #[test]
    fn parse_trims_nesting_whitespace() {
        let dir = tempfile::TempDir::new().unwrap();
        let cwd = dir.path();

        let h1 = cwd.join("nested.h");
        let source = cwd.join("main.cpp");
        std::fs::write(&h1, "").unwrap();
        std::fs::write(&source, "").unwrap();

        // Deep nesting: many spaces between prefix and path
        let stderr = format!("Note: including file:               {}\r\n", h1.display());

        let (scan, _) = parse_show_includes(stderr.as_bytes(), &source, cwd);
        assert_eq!(scan.resolved.len(), 1);
    }

    #[test]
    fn has_computed_always_false() {
        let dir = tempfile::TempDir::new().unwrap();
        let cwd = dir.path();
        let source = cwd.join("main.cpp");
        std::fs::write(&source, "").unwrap();

        // Even with no /showIncludes lines, has_computed is false.
        let (scan, _) = parse_show_includes(b"some warning\r\n", &source, cwd);
        assert!(!scan.has_computed);
    }

    #[test]
    fn preserves_empty_lines() {
        let dir = tempfile::TempDir::new().unwrap();
        let cwd = dir.path();

        let h1 = cwd.join("a.h");
        let source = cwd.join("main.cpp");
        std::fs::write(&h1, "").unwrap();
        std::fs::write(&source, "").unwrap();

        let stderr = format!(
            "Note: including file: {}\r\nfirst\r\n\r\nsecond\r\n",
            h1.display(),
        );

        let (_, filtered) = parse_show_includes(stderr.as_bytes(), &source, cwd);
        assert_eq!(filtered, b"first\r\n\r\nsecond\r\n");
    }

    #[test]
    fn preserves_crlf_endings() {
        let dir = tempfile::TempDir::new().unwrap();
        let cwd = dir.path();

        let h1 = cwd.join("a.h");
        let source = cwd.join("main.cpp");
        std::fs::write(&h1, "").unwrap();
        std::fs::write(&source, "").unwrap();

        let stderr = format!("Note: including file: {}\r\nwarning\r\n", h1.display(),);

        let (_, filtered) = parse_show_includes(stderr.as_bytes(), &source, cwd);
        assert_eq!(filtered, b"warning\r\n");
    }

    #[test]
    fn non_english_locale_detected() {
        if !crate::platform::host::is_windows() {
            return;
        }
        // Simulates Japanese MSVC locale.
        let dir = tempfile::TempDir::new().unwrap();
        let cwd = dir.path();

        let h1 = cwd.join("stdio.h");
        let h2 = cwd.join("stddef.h");
        let source = cwd.join("main.cpp");
        std::fs::write(&h1, "").unwrap();
        std::fs::write(&h2, "").unwrap();
        std::fs::write(&source, "").unwrap();

        let stderr = format!(
            "ãƒ¡ãƒ¢: ã‚¤ãƒ³ã‚¯ãƒ«ãƒ¼ãƒ‰ ãƒ•ã‚¡ã‚¤ãƒ«: {}\r\nãƒ¡ãƒ¢: ã‚¤ãƒ³ã‚¯ãƒ«ãƒ¼ãƒ‰ ãƒ•ã‚¡ã‚¤ãƒ«: {}\r\n",
            h1.display(),
            h2.display(),
        );

        let (scan, filtered) = parse_show_includes(stderr.as_bytes(), &source, cwd);
        assert_eq!(scan.resolved.len(), 2);
        assert!(filtered.is_empty());
    }

    #[test]
    fn prefix_detection_ignores_error_paths() {
        // Error lines with paths at the start should not be detected as /showIncludes.
        let line = "C:\\src\\main.cpp(10): error C2065: 'foo': undeclared identifier";
        assert!(extract_prefix_candidate(line).is_none());
    }

    #[test]
    fn prefix_candidate_english() {
        let line = "Note: including file: C:\\Windows\\stdio.h";
        assert_eq!(
            extract_prefix_candidate(line),
            Some("Note: including file:".to_string())
        );
    }

    #[test]
    fn prefix_candidate_japanese() {
        let line = "ãƒ¡ãƒ¢: ã‚¤ãƒ³ã‚¯ãƒ«ãƒ¼ãƒ‰ ãƒ•ã‚¡ã‚¤ãƒ«: C:\\Windows\\stdio.h";
        assert_eq!(
            extract_prefix_candidate(line),
            Some("ãƒ¡ãƒ¢: ã‚¤ãƒ³ã‚¯ãƒ«ãƒ¼ãƒ‰ ãƒ•ã‚¡ã‚¤ãƒ«:".to_string())
        );
    }

    #[test]
    fn prefix_candidate_chinese() {
        let line = "æ³¨æ„: åŒ…å«æ–‡ä»¶: C:\\Windows\\stdio.h";
        assert_eq!(
            extract_prefix_candidate(line),
            Some("æ³¨æ„: åŒ…å«æ–‡ä»¶:".to_string())
        );
    }
}

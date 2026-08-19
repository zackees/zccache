//! GCC/Clang `-H` include-trace parsing and stderr filtering.

use std::collections::HashSet;
use std::path::Path;

use zccache_core::NormalizedPath;

use crate::scanner::ScanResult;

const MAX_PARTIAL_LINE_BYTES: usize = 64 * 1024;

/// Incremental parser for compiler-emitted `-H` records.
pub struct HeaderTraceParser {
    source: NormalizedPath,
    cwd: NormalizedPath,
    seen: HashSet<NormalizedPath>,
    resolved: Vec<NormalizedPath>,
    partial: Vec<u8>,
    passthrough_line: bool,
    gcc_guard_summary: bool,
    incomplete: bool,
}

impl HeaderTraceParser {
    #[must_use]
    pub fn new(source: &Path, cwd: &Path) -> Self {
        Self {
            source: crate::depfile::canonicalize_path(source, cwd),
            cwd: NormalizedPath::new(cwd),
            seen: HashSet::new(),
            resolved: Vec::new(),
            partial: Vec::new(),
            passthrough_line: false,
            gcc_guard_summary: false,
            incomplete: false,
        }
    }

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

    #[must_use]
    pub fn finish(mut self, output: &mut Vec<u8>) -> ScanResult {
        if !self.partial.is_empty() {
            self.finish_line(output);
        }
        ScanResult {
            resolved: self.resolved,
            unresolved: Vec::new(),
            has_computed: self.incomplete,
        }
    }

    fn finish_line(&mut self, output: &mut Vec<u8>) {
        let raw = std::mem::take(&mut self.partial);
        if !self.record_header(&raw) {
            output.extend_from_slice(&raw);
        }
    }

    fn record_header(&mut self, raw: &[u8]) -> bool {
        let text = raw.strip_suffix(b"\n").unwrap_or(raw);
        let text = text.strip_suffix(b"\r").unwrap_or(text);
        if text == b"Multiple include guards may be useful for:" {
            self.gcc_guard_summary = true;
            return true;
        }
        if self.gcc_guard_summary {
            if self.record_path_bytes(text) {
                return true;
            }
            if looks_like_path(text) {
                self.incomplete = true;
            }
            self.gcc_guard_summary = false;
        }
        let depth = text.iter().take_while(|byte| **byte == b'.').count();
        let path_start = match text.get(depth..) {
            Some([b' ', ..]) if depth > 0 => depth + 1,
            Some([b'!' | b'x', b' ', ..]) if depth > 0 => depth + 2,
            _ => return false,
        };
        if self.record_path_bytes(&text[path_start..]) {
            true
        } else {
            // The line has the compiler's exact trace shape, but zccache
            // could not retain its path. Keep the bytes visible as a possible
            // diagnostic and disable direct-mode hits for this manifest.
            self.incomplete = true;
            false
        }
    }

    fn record_path_bytes(&mut self, bytes: &[u8]) -> bool {
        std::str::from_utf8(bytes)
            .ok()
            .is_some_and(|path| self.record_path(Path::new(path)))
    }

    fn record_path(&mut self, path: &Path) -> bool {
        if path.as_os_str().is_empty() {
            return false;
        }
        let path = if path.is_absolute() {
            crate::depfile::canonicalize_path(path, self.cwd.as_path())
        } else {
            crate::depfile::canonicalize_path(&self.cwd.join(path), self.cwd.as_path())
        };
        // `-H` records name files that the compiler successfully opened. An
        // existence check prevents trace-shaped diagnostics from being
        // swallowed if they do not name an actual include.
        if !path.is_file() {
            return false;
        }
        if path != self.source && self.seen.insert(path.clone()) {
            self.resolved.push(path);
        }
        true
    }
}

fn looks_like_path(bytes: &[u8]) -> bool {
    bytes
        .first()
        .is_some_and(|byte| matches!(*byte, b'/' | b'\\'))
        || bytes.get(1) == Some(&b':')
        || bytes.iter().any(|byte| matches!(*byte, b'/' | b'\\'))
}

#[must_use]
pub fn parse_header_trace(stderr: &[u8], source: &Path, cwd: &Path) -> (ScanResult, Vec<u8>) {
    let mut parser = HeaderTraceParser::new(source, cwd);
    let mut filtered = Vec::new();
    parser.push(stderr, &mut filtered);
    (parser.finish(&mut filtered), filtered)
}

/// Parse Clang's private `-header-include-file` output.
///
/// Unlike `-H`, this sink contains one bare path per line and does not share
/// stderr with user diagnostics. Missing or malformed output fails closed so
/// a compiler-version mismatch can never create a direct-mode false hit.
#[must_use]
pub fn parse_header_trace_file(path: &Path, source: &Path, cwd: &Path) -> ScanResult {
    let mut parser = HeaderTraceParser::new(source, cwd);
    let trace = match std::fs::read(path) {
        Ok(trace) => trace,
        Err(_) => {
            parser.incomplete = true;
            return parser.finish(&mut Vec::new());
        }
    };
    for line in trace.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if !line.is_empty() && !parser.record_path_bytes(line) {
            parser.incomplete = true;
        }
    }
    parser.finish(&mut Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_paths_with_spaces_and_preserves_diagnostics() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("main.c");
        let header = temp.path().join("header with spaces.h");
        std::fs::write(&source, "").unwrap();
        std::fs::write(&header, "").unwrap();
        let stderr = format!(
            ". {}\nwarning: useful diagnostic\n.. {}\n",
            header.display(),
            header.display()
        );

        let (scan, filtered) = parse_header_trace(stderr.as_bytes(), &source, temp.path());

        assert_eq!(
            scan.resolved,
            vec![crate::depfile::canonicalize_path(&header, temp.path())]
        );
        assert_eq!(filtered, b"warning: useful diagnostic\n");
        assert!(!scan.has_computed);
    }

    #[test]
    fn streaming_chunks_match_batch_output() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("main.c");
        let header = temp.path().join("header.h");
        std::fs::write(&source, "").unwrap();
        std::fs::write(&header, "").unwrap();
        let stderr = format!(". {}\r\nnote: retained\r\n", header.display());
        let expected = parse_header_trace(stderr.as_bytes(), &source, temp.path());
        let mut parser = HeaderTraceParser::new(&source, temp.path());
        let mut filtered = Vec::new();
        for chunk in stderr.as_bytes().chunks(3) {
            parser.push(chunk, &mut filtered);
        }
        let scan = parser.finish(&mut filtered);

        assert_eq!(scan.resolved, expected.0.resolved);
        assert_eq!(filtered, expected.1);
    }

    #[test]
    fn malformed_trace_shaped_lines_are_not_removed() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("main.c");
        std::fs::write(&source, "").unwrap();
        let stderr = b". not a path\n...\n.. warning: still useful\n";

        let (scan, filtered) = parse_header_trace(stderr, &source, temp.path());

        assert!(scan.resolved.is_empty());
        assert_eq!(filtered, stderr);
        assert!(scan.has_computed);
    }

    #[test]
    fn source_record_is_filtered_but_not_tracked() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("main.c");
        std::fs::write(&source, "").unwrap();
        let stderr = format!(". {}\n", source.display());

        let (scan, filtered) = parse_header_trace(stderr.as_bytes(), &source, temp.path());

        assert!(scan.resolved.is_empty());
        assert!(filtered.is_empty());
    }

    #[test]
    fn filters_gcc_include_guard_advisory_records_only() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("main.c");
        let header = temp.path().join("unguarded.h");
        std::fs::write(&source, "").unwrap();
        std::fs::write(&header, "").unwrap();
        let stderr = format!(
            "Multiple include guards may be useful for:\n{}\nwarning: retained\n",
            header.display()
        );

        let (scan, filtered) = parse_header_trace(stderr.as_bytes(), &source, temp.path());

        assert_eq!(
            scan.resolved,
            vec![crate::depfile::canonicalize_path(&header, temp.path())]
        );
        assert_eq!(filtered, b"warning: retained\n");
    }

    #[test]
    fn disappeared_trace_path_fails_closed_and_remains_visible() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("main.c");
        std::fs::write(&source, "").unwrap();
        let missing = temp.path().join("removed-system-header.h");
        let stderr = format!(". {}\n", missing.display());

        let (scan, filtered) = parse_header_trace(stderr.as_bytes(), &source, temp.path());

        assert!(scan.resolved.is_empty());
        assert!(scan.has_computed);
        assert_eq!(filtered, stderr.as_bytes());
    }

    #[test]
    fn distinct_non_utf8_header_paths_fail_closed_without_lossy_deduplication() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("main.c");
        let Some(first_name) = crate::platform::fs::path::from_raw_bytes(b"header-\xff.h") else {
            return;
        };
        let Some(second_name) = crate::platform::fs::path::from_raw_bytes(b"header-\xfe.h") else {
            return;
        };
        let first = temp.path().join(first_name);
        let second = temp.path().join(second_name);
        std::fs::write(&source, "").unwrap();
        std::fs::write(&first, "").unwrap();
        std::fs::write(&second, "").unwrap();
        let stderr = b". header-\xff.h\n. header-\xfe.h\n".to_vec();

        let (scan, filtered) = parse_header_trace(&stderr, &source, temp.path());

        assert!(scan.resolved.is_empty());
        assert!(scan.has_computed);
        assert_eq!(filtered, stderr);
    }

    #[test]
    fn gcc_pch_markers_are_filtered_and_tracked() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("main.c");
        let valid = temp.path().join("valid.h.gch");
        let invalid = temp.path().join("invalid.h.gch");
        std::fs::write(&source, "").unwrap();
        std::fs::write(&valid, "").unwrap();
        std::fs::write(&invalid, "").unwrap();
        let stderr = format!("..! {}\n...x {}\n", valid.display(), invalid.display());

        let (scan, filtered) = parse_header_trace(stderr.as_bytes(), &source, temp.path());

        assert_eq!(
            scan.resolved,
            vec![NormalizedPath::new(&valid), NormalizedPath::new(&invalid)]
        );
        assert!(!scan.has_computed);
        assert!(filtered.is_empty());

        let mut parser = HeaderTraceParser::new(&source, temp.path());
        let mut streamed = Vec::new();
        for chunk in stderr.as_bytes().chunks(2) {
            parser.push(chunk, &mut streamed);
        }
        let streamed_scan = parser.finish(&mut streamed);
        assert_eq!(streamed_scan.resolved, scan.resolved);
        assert!(streamed.is_empty());
    }

    #[test]
    fn private_trace_file_tracks_paths_and_fails_closed_on_bad_records() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("main.c");
        let header = temp.path().join("header with spaces.h");
        let trace = temp.path().join("main.headers");
        std::fs::write(&source, "").unwrap();
        std::fs::write(&header, "").unwrap();
        std::fs::write(
            &trace,
            format!("{}\n{}\nmissing.h\n", source.display(), header.display()),
        )
        .unwrap();

        let scan = parse_header_trace_file(&trace, &source, temp.path());

        assert_eq!(
            scan.resolved,
            vec![crate::depfile::canonicalize_path(&header, temp.path())]
        );
        assert!(scan.has_computed);
    }

    #[test]
    fn missing_private_trace_file_fails_closed() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("main.c");
        std::fs::write(&source, "").unwrap();

        let scan =
            parse_header_trace_file(&temp.path().join("missing.headers"), &source, temp.path());

        assert!(scan.resolved.is_empty());
        assert!(scan.has_computed);
    }
}

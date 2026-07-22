//! Clang/GCC `-H` header-trace parser.
//!
//! `-H` prints one resolved header per stderr line, indented by include depth.
//! zccache injects it only for the private Clang MMD depfile path: Clang then
//! writes the same complete graph to that depfile, while this parser removes
//! the diagnostic-only trace from the compiler output returned to the caller.

use std::collections::HashSet;
use std::path::Path;

use zccache_core::NormalizedPath;

use super::depfile::canonicalize_path;
use super::scanner::ScanResult;

/// Keep the embedded streaming path bounded when a compiler emits malformed
/// stderr without a newline.
const MAX_PARTIAL_LINE_BYTES: usize = 64 * 1024;

/// Incremental parser for compiler `-H` stderr output.
pub struct HeaderTraceParser {
    source_canonical: NormalizedPath,
    cwd: NormalizedPath,
    seen: HashSet<NormalizedPath>,
    resolved: Vec<NormalizedPath>,
    partial: Vec<u8>,
}

impl HeaderTraceParser {
    /// Create a parser that removes only recognized, existing header paths.
    pub fn new(source: &Path, cwd: &Path) -> Self {
        Self {
            source_canonical: canonicalize_path(source, cwd),
            cwd: NormalizedPath::new(cwd),
            seen: HashSet::new(),
            resolved: Vec::new(),
            partial: Vec::new(),
        }
    }

    /// Consume stderr bytes, appending non-trace output unchanged to `output`.
    pub fn push(&mut self, mut chunk: &[u8], output: &mut Vec<u8>) {
        while let Some(newline) = chunk.iter().position(|byte| *byte == b'\n') {
            self.partial.extend_from_slice(&chunk[..=newline]);
            chunk = &chunk[newline + 1..];
            self.finish_line(output);
        }
        self.partial.extend_from_slice(chunk);
        if self.partial.len() > MAX_PARTIAL_LINE_BYTES {
            output.extend_from_slice(&self.partial);
            self.partial.clear();
        }
    }

    /// Flush the final partial line and return the compiler-resolved headers.
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
        let text = raw
            .strip_suffix(b"\n")
            .unwrap_or(&raw)
            .strip_suffix(b"\r")
            .unwrap_or_else(|| raw.strip_suffix(b"\n").unwrap_or(&raw));
        let line = String::from_utf8_lossy(text);
        let Some(path) = trace_path(&line) else {
            output.extend_from_slice(&raw);
            return;
        };
        let path = Path::new(path);
        if !path.is_file() {
            output.extend_from_slice(&raw);
            return;
        }
        let resolved = if path.is_absolute() {
            canonicalize_path(path, self.cwd.as_path())
        } else {
            canonicalize_path(&self.cwd.join(path), self.cwd.as_path())
        };
        if resolved != self.source_canonical && self.seen.insert(resolved.clone()) {
            self.resolved.push(resolved);
        }
    }
}

/// Parse and remove `-H` trace lines from a completed compiler stderr stream.
pub fn parse_header_trace(stderr: &[u8], source: &Path, cwd: &Path) -> (ScanResult, Vec<u8>) {
    let mut parser = HeaderTraceParser::new(source, cwd);
    let mut filtered = Vec::new();
    parser.push(stderr, &mut filtered);
    let scan = parser.finish(&mut filtered);
    (scan, filtered)
}

fn trace_path(line: &str) -> Option<&str> {
    let without_depth = line.trim_start_matches('.');
    if without_depth.len() == line.len() {
        return None;
    }
    let path = without_depth.trim_start();
    (!path.is_empty()).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_filters_header_trace_without_hiding_diagnostics() {
        let dir = tempfile::TempDir::new().unwrap();
        let source = dir.path().join("main.c");
        let header = dir.path().join("header.h");
        std::fs::write(&source, "").unwrap();
        std::fs::write(&header, "").unwrap();
        let stderr = format!(
            ". {}\n.. not-a-header\nwarning: retain this\n",
            header.display()
        );

        let (scan, filtered) = parse_header_trace(stderr.as_bytes(), &source, dir.path());

        assert_eq!(scan.resolved, vec![canonicalize_path(&header, dir.path())]);
        assert_eq!(filtered, b".. not-a-header\nwarning: retain this\n");
    }
}

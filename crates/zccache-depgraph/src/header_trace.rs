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
        let Some(path) = self.resolve_path(path) else {
            return false;
        };
        self.record_resolved_path(path);
        true
    }

    fn resolve_path(&self, path: &Path) -> Option<NormalizedPath> {
        if path.as_os_str().is_empty() {
            return None;
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
            return None;
        }
        Some(path)
    }

    fn record_resolved_path(&mut self, path: NormalizedPath) {
        if path != self.source && self.seen.insert(path.clone()) {
            self.resolved.push(path);
        }
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
/// The file contains one compiler-opened header path per line. Pairing it with
/// `-sys-header-deps` retains the same user + system header set as `-H` without
/// routing trace records through the diagnostic stream or constructing a DOT
/// graph. Missing, overlong, or malformed records fail closed.
#[must_use]
pub fn parse_header_include_file(path: &Path, source: &Path, cwd: &Path) -> ScanResult {
    use std::io::BufRead as _;

    let mut parser = HeaderTraceParser::new(source, cwd);
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(_) => {
            parser.incomplete = true;
            return parser.finish(&mut Vec::new());
        }
    };
    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {
                if line.len() > MAX_PARTIAL_LINE_BYTES {
                    parser.incomplete = true;
                    continue;
                }
                let record = line.strip_suffix(b"\n").unwrap_or(&line);
                let record = record.strip_suffix(b"\r").unwrap_or(record);
                if record.is_empty() || !parser.record_path_bytes(record) {
                    parser.incomplete = true;
                }
            }
            Err(_) => {
                parser.incomplete = true;
                break;
            }
        }
    }
    parser.finish(&mut Vec::new())
}

/// Parse Clang's private `-dependency-dot` output.
///
/// This sink contains compiler-selected user and system headers without
/// sharing stderr with diagnostics. Missing or malformed output fails closed
/// so a compiler-version mismatch can never create a direct-mode false hit.
#[must_use]
pub fn parse_dependency_graph_file(path: &Path, source: &Path, cwd: &Path) -> ScanResult {
    let mut parser = HeaderTraceParser::new(source, cwd);
    let graph = match std::fs::read(path) {
        Ok(graph) => graph,
        Err(_) => {
            parser.incomplete = true;
            return parser.finish(&mut Vec::new());
        }
    };
    let mut saw_header = false;
    let mut saw_footer = false;
    let mut saw_source = false;
    let mut node_ids = HashSet::new();
    let mut edges = Vec::new();
    for line in graph.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        if !saw_header {
            saw_header = line == b"digraph \"dependencies\" {";
            parser.incomplete |= !saw_header;
            continue;
        }
        if line == b"}" {
            if saw_footer {
                parser.incomplete = true;
            }
            saw_footer = true;
            continue;
        }
        if saw_footer {
            parser.incomplete = true;
            continue;
        }
        let Some(line) = line.strip_prefix(b"  ") else {
            parser.incomplete = true;
            continue;
        };
        if let Some(edge) = parse_dependency_graph_edge(line) {
            edges.push(edge);
            continue;
        }
        let Some((id, label)) = parse_dependency_graph_node(line) else {
            parser.incomplete = true;
            continue;
        };
        if !node_ids.insert(id) {
            parser.incomplete = true;
            continue;
        }
        let Some(path) = resolve_dependency_graph_path(&parser, &label) else {
            parser.incomplete = true;
            continue;
        };
        if path == parser.source {
            saw_source = true;
        }
        parser.record_resolved_path(path);
    }
    // Clang emits a syntactically complete empty graph for a translation
    // unit with no includes. Once any node exists, however, the source node
    // is required so a truncated graph cannot be mistaken for a complete
    // dependency manifest.
    parser.incomplete |= !saw_header
        || !saw_footer
        || (!node_ids.is_empty() && !saw_source)
        || edges
            .iter()
            .any(|(from, to)| !node_ids.contains(from) || !node_ids.contains(to));
    parser.finish(&mut Vec::new())
}

fn parse_dependency_graph_node(line: &[u8]) -> Option<(u64, Vec<u8>)> {
    let line = line.strip_prefix(b"header_")?;
    let id_len = line.iter().take_while(|byte| byte.is_ascii_digit()).count();
    if id_len == 0 {
        return None;
    }
    let id = std::str::from_utf8(&line[..id_len]).ok()?.parse().ok()?;
    let line = &line[id_len..];
    let prefix = b" [ shape=\"box\", label=\"";
    let mut index = prefix.len();
    line.starts_with(prefix).then_some(())?;
    let mut label = Vec::new();
    while index < line.len() {
        match line[index] {
            b'"' => {
                return (line.get(index + 1..) == Some(b"];")).then_some((id, label));
            }
            b'\\' => {
                index += 1;
                let escaped = *line.get(index)?;
                match escaped {
                    b'n' => label.push(b'\n'),
                    b'\\' | b'"' | b'{' | b'}' | b'<' | b'>' | b'|' => label.push(escaped),
                    // LLVM deliberately leaves `\l` untouched.
                    b'l' => label.extend_from_slice(b"\\l"),
                    _ => return None,
                }
            }
            byte => label.push(byte),
        }
        index += 1;
    }
    None
}

fn parse_dependency_graph_edge(line: &[u8]) -> Option<(u64, u64)> {
    let line = line.strip_prefix(b"header_")?;
    let from_len = line.iter().take_while(|byte| byte.is_ascii_digit()).count();
    if from_len == 0 {
        return None;
    }
    let from = std::str::from_utf8(&line[..from_len]).ok()?.parse().ok()?;
    let line = line[from_len..].strip_prefix(b" -> header_")?;
    let to_len = line.iter().take_while(|byte| byte.is_ascii_digit()).count();
    if to_len == 0 || line.get(to_len..) != Some(b";") {
        return None;
    }
    let to = std::str::from_utf8(&line[..to_len]).ok()?.parse().ok()?;
    Some((from, to))
}

fn resolve_dependency_graph_path(
    parser: &HeaderTraceParser,
    bytes: &[u8],
) -> Option<NormalizedPath> {
    let path = Path::new(std::str::from_utf8(bytes).ok()?);
    let mut candidates = Vec::new();
    if let Some(candidate) = parser.resolve_path(path) {
        candidates.push(candidate);
    }
    if !path.is_absolute() {
        if let Some(candidate) = crate::platform::fs::path::system_root_candidate(path)
            .and_then(|rooted| parser.resolve_path(&rooted))
        {
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
    }
    if candidates.len() == 1 {
        candidates.pop()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_private_header_include_file_with_system_paths() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("main.cpp");
        let user = temp.path().join("user.hpp");
        let system = temp.path().join("system.hpp");
        let trace = temp.path().join("headers.txt");
        std::fs::write(&source, "").unwrap();
        std::fs::write(&user, "").unwrap();
        std::fs::write(&system, "").unwrap();
        std::fs::write(
            &trace,
            format!(
                "{}\n{}\n{}\n",
                user.display(),
                system.display(),
                user.display()
            ),
        )
        .unwrap();

        let scan = parse_header_include_file(&trace, &source, temp.path());

        assert_eq!(
            scan.resolved,
            vec![
                crate::depfile::canonicalize_path(&user, temp.path()),
                crate::depfile::canonicalize_path(&system, temp.path()),
            ]
        );
        assert!(!scan.has_computed);
    }

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
        // Some Unix filesystems reject byte sequences that `OsString` can
        // represent (APFS reports EILSEQ, for example). Exercise the
        // fail-closed path wherever the fixture itself is supported.
        if std::fs::write(&first, "").is_err() || std::fs::write(&second, "").is_err() {
            return;
        }
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
            vec![
                crate::depfile::canonicalize_path(&valid, temp.path()),
                crate::depfile::canonicalize_path(&invalid, temp.path()),
            ]
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
    fn private_dependency_graph_tracks_paths_and_fails_closed_on_bad_records() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("main.c");
        let header = temp.path().join("header with spaces.h");
        let trace = temp.path().join("main.headers");
        std::fs::write(&source, "").unwrap();
        std::fs::write(&header, "").unwrap();
        std::fs::write(
            &trace,
            format!(
                "digraph \"dependencies\" {{\n  header_0 [ shape=\"box\", label=\"{}\"];\n  header_1 [ shape=\"box\", label=\"{}\"];\n  malformed node\n}}\n",
                dot_escape_path(&source),
                dot_escape_path(&header)
            ),
        )
        .unwrap();

        let scan = parse_dependency_graph_file(&trace, &source, temp.path());

        assert_eq!(
            scan.resolved,
            vec![crate::depfile::canonicalize_path(&header, temp.path())]
        );
        assert!(scan.has_computed);
    }

    #[test]
    fn rootless_dependency_graph_paths_resolve_from_the_system_root() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("main.c");
        let header = temp.path().join("system.h");
        std::fs::write(&source, "").unwrap();
        std::fs::write(&header, "").unwrap();
        let rootless = header
            .to_string_lossy()
            .trim_start_matches(['/', '\\'])
            .to_string();
        let Some(candidate) =
            crate::platform::fs::path::system_root_candidate(Path::new(&rootless))
        else {
            return;
        };
        if !candidate.is_file() {
            return;
        }
        let trace = temp.path().join("rootless.headers");
        std::fs::write(
            &trace,
            format!(
                "digraph \"dependencies\" {{\n  header_0 [ shape=\"box\", label=\"{}\"];\n  header_1 [ shape=\"box\", label=\"{}\"];\n  header_0 -> header_1;\n}}\n",
                dot_escape_path(&source),
                rootless.replace('\\', "\\\\").replace('"', "\\\"")
            ),
        )
        .unwrap();

        let scan = parse_dependency_graph_file(&trace, &source, temp.path());

        assert_eq!(
            scan.resolved,
            vec![crate::depfile::canonicalize_path(&header, temp.path())]
        );
        assert!(!scan.has_computed);
    }

    #[test]
    fn ambiguous_rootless_dependency_graph_path_fails_closed() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("main.c");
        let header = temp.path().join("system.h");
        std::fs::write(&source, "").unwrap();
        std::fs::write(&header, "").unwrap();
        let rootless = header
            .to_string_lossy()
            .trim_start_matches(['/', '\\'])
            .to_string();
        let Some(candidate) =
            crate::platform::fs::path::system_root_candidate(Path::new(&rootless))
        else {
            return;
        };
        if !candidate.is_file() {
            return;
        }
        let shadow = temp.path().join(&rootless);
        std::fs::create_dir_all(shadow.parent().unwrap()).unwrap();
        std::fs::write(&shadow, "shadow").unwrap();
        let trace = temp.path().join("ambiguous.headers");
        std::fs::write(
            &trace,
            format!(
                "digraph \"dependencies\" {{\n  header_0 [ shape=\"box\", label=\"{}\"];\n  header_1 [ shape=\"box\", label=\"{}\"];\n  header_0 -> header_1;\n}}\n",
                dot_escape_path(&source),
                rootless.replace('\\', "\\\\").replace('"', "\\\"")
            ),
        )
        .unwrap();

        let scan = parse_dependency_graph_file(&trace, &source, temp.path());

        assert!(scan.resolved.is_empty());
        assert!(scan.has_computed);
    }

    #[test]
    fn dependency_graph_requires_nodes_source_and_one_footer() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("main.c");
        std::fs::write(&source, "").unwrap();
        for graph in [
            "digraph \"dependencies\" {\n  header_0 -> header_1;\n}\n",
            "digraph \"dependencies\" {\n  header_0 -> broken;\n}\n",
        ] {
            let trace = temp.path().join("invalid.headers");
            std::fs::write(&trace, graph).unwrap();

            let scan = parse_dependency_graph_file(&trace, &source, temp.path());

            assert!(scan.resolved.is_empty());
            assert!(scan.has_computed, "graph should fail closed: {graph:?}");
        }

        let trace = temp.path().join("duplicate-footer.headers");
        let graph = format!(
            "digraph \"dependencies\" {{\n  header_0 [ shape=\"box\", label=\"{}\"];\n}}\n}}\n",
            dot_escape_path(&source)
        );
        std::fs::write(&trace, &graph).unwrap();

        let scan = parse_dependency_graph_file(&trace, &source, temp.path());

        assert!(scan.resolved.is_empty());
        assert!(scan.has_computed, "graph should fail closed: {graph:?}");
    }

    #[test]
    fn empty_dependency_graph_is_a_complete_no_include_manifest() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("main.c");
        let trace = temp.path().join("empty.headers");
        std::fs::write(&source, "int main(void) { return 0; }\n").unwrap();
        std::fs::write(&trace, "digraph \"dependencies\" {\n}\n").unwrap();

        let scan = parse_dependency_graph_file(&trace, &source, temp.path());

        assert!(scan.resolved.is_empty());
        assert!(!scan.has_computed);
    }

    #[test]
    fn missing_private_dependency_graph_fails_closed() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("main.c");
        std::fs::write(&source, "").unwrap();

        let scan =
            parse_dependency_graph_file(&temp.path().join("missing.headers"), &source, temp.path());

        assert!(scan.resolved.is_empty());
        assert!(scan.has_computed);
    }

    fn dot_escape_path(path: &Path) -> String {
        path.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    }
}

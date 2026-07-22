//! Clang private header-trace parser.
//!
//! Clang's `-header-include-file <path> -sys-header-deps` frontend arguments
//! write one compiler-resolved header path per line. zccache injects them only
//! with its private MMD depfile, so normal compiler diagnostics stay untouched.

use std::collections::HashSet;
use std::path::Path;

use zccache_core::NormalizedPath;

use super::depfile::canonicalize_path;
use super::scanner::ScanResult;

/// Parse a private Clang header trace file.
pub fn parse_header_trace(path: &Path, source: &Path, cwd: &Path) -> ScanResult {
    let source_canonical = canonicalize_path(source, cwd);
    let cwd = NormalizedPath::new(cwd);
    let mut seen = HashSet::new();
    let mut resolved = Vec::new();
    let trace = std::fs::read_to_string(path).unwrap_or_default();
    for line in trace.lines().filter(|line| !line.is_empty()) {
        let header = Path::new(line);
        if !header.is_file() {
            continue;
        }
        let header = if header.is_absolute() {
            canonicalize_path(header, cwd.as_path())
        } else {
            canonicalize_path(&cwd.join(header), cwd.as_path())
        };
        if header != source_canonical && seen.insert(header.clone()) {
            resolved.push(header);
        }
    }
    ScanResult {
        resolved,
        unresolved: Vec::new(),
        has_computed: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_private_header_trace_paths() {
        let dir = tempfile::TempDir::new().unwrap();
        let source = dir.path().join("main.c");
        let header = dir.path().join("header.h");
        let spaced_header = dir.path().join("header with spaces.h");
        std::fs::write(&source, "").unwrap();
        std::fs::write(&header, "").unwrap();
        std::fs::write(&spaced_header, "").unwrap();
        let trace = dir.path().join("headers.trace");
        std::fs::write(
            &trace,
            format!(
                "{}\n{}\nmissing.h\nmalformed trace record\n",
                header.display(),
                spaced_header.display()
            ),
        )
        .unwrap();
        let scan = parse_header_trace(&trace, &source, dir.path());

        assert_eq!(
            scan.resolved,
            vec![
                canonicalize_path(&header, dir.path()),
                canonicalize_path(&spaced_header, dir.path()),
            ]
        );
    }
}

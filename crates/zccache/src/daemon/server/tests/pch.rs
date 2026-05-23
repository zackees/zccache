//! Tests for the precompiled-header source resolver — both the
//! filesystem walker (`pch_source_header`) and the registry-fast-path
//! wrapper (`resolve_pch_source`).

use std::path::Path;

use super::super::*;

// ── pch_source_header unit tests ────────────────────────────────────

#[test]
fn pch_source_header_sibling() {
    let tmp = tempfile::tempdir().unwrap();
    let header = tmp.path().join("pch.h");
    let pch = tmp.path().join("pch.h.pch");
    std::fs::write(&header, "// pch").unwrap();
    std::fs::write(&pch, "binary").unwrap();

    let result = pch_source_header(&pch);
    assert_eq!(result, Some(header.into()));
}

#[test]
fn pch_source_header_build_dir() {
    // The walk-up heuristic looks for `<dir_name>/<header_name>` from ancestors.
    // e.g., for .build/tests/pch.h.pch it looks for tests/pch.h in parents.
    let tmp = tempfile::tempdir().unwrap();
    // Source: tmp/tests/pch.h (matches the `tests/pch.h` relative lookup)
    let src_dir = tmp.path().join("tests");
    std::fs::create_dir_all(&src_dir).unwrap();
    let header = src_dir.join("pch.h");
    std::fs::write(&header, "// pch").unwrap();

    // PCH: tmp/build/tests/pch.h.pch
    let build_dir = tmp.path().join("build").join("tests");
    std::fs::create_dir_all(&build_dir).unwrap();
    let pch = build_dir.join("pch.h.pch");
    std::fs::write(&pch, "binary").unwrap();

    let result = pch_source_header(&pch);
    assert_eq!(result, Some(header.into()));
}

#[test]
fn pch_source_header_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let build_dir = tmp.path().join("build");
    std::fs::create_dir_all(&build_dir).unwrap();
    let pch = build_dir.join("pch.h.pch");
    std::fs::write(&pch, "binary").unwrap();

    let result = pch_source_header(&pch);
    assert_eq!(result, None);
}

#[test]
fn pch_source_header_non_pch() {
    let tmp = tempfile::tempdir().unwrap();
    let obj = tmp.path().join("foo.o");
    std::fs::write(&obj, "object").unwrap();

    let result = pch_source_header(&obj);
    assert_eq!(result, None);
}

#[test]
fn pch_source_header_gch_extension() {
    let tmp = tempfile::tempdir().unwrap();
    let header = tmp.path().join("pch.h");
    let gch = tmp.path().join("pch.h.gch");
    std::fs::write(&header, "// pch").unwrap();
    std::fs::write(&gch, "binary").unwrap();

    let result = pch_source_header(&gch);
    assert_eq!(result, Some(header.into()));
}

// ── resolve_pch_source unit tests ───────────────────────────────────

#[test]
fn resolve_pch_source_registry_hit() {
    let pch_map: DashMap<NormalizedPath, NormalizedPath> = DashMap::new();
    let pch_path = NormalizedPath::from("/build/tests/pch.h.pch");
    let src_path = NormalizedPath::from("/src/tests/pch.h");
    pch_map.insert(pch_path.clone(), src_path.clone());

    let result = resolve_pch_source(&pch_path, &pch_map);
    assert_eq!(result, Some(src_path));
}

#[test]
fn resolve_pch_source_falls_back_to_filesystem() {
    let tmp = tempfile::tempdir().unwrap();
    let header = tmp.path().join("pch.h");
    let pch = tmp.path().join("pch.h.pch");
    std::fs::write(&header, "// pch").unwrap();
    std::fs::write(&pch, "binary").unwrap();

    let pch_map: DashMap<NormalizedPath, NormalizedPath> = DashMap::new();
    let result = resolve_pch_source(&pch, &pch_map);
    assert_eq!(result, Some(header.into()));
}

#[test]
fn resolve_pch_source_non_pch_returns_none() {
    let pch_map: DashMap<NormalizedPath, NormalizedPath> = DashMap::new();
    let result = resolve_pch_source(Path::new("/build/foo.o"), &pch_map);
    assert_eq!(result, None);
}

//! Adversarial expansion tests for response file `@file` recursion + file content.
//!
//! Targets dark corners of `expand_response_files`:
//! - Expansion depth/cycle boundaries (exact MAX_DEPTH, off-by-one)
//! - Circular reference variants (self, mutual, triangle, deep-cycle)
//! - File content edge cases (empty, whitespace-only, BOM, CRLF)
//! - Stress: large files, deep chains, many siblings
//!
//! Run all:    soldr cargo test -p zccache --test compiler_response_file_expansion_test -- --nocapture
//! Run single: soldr cargo test -p zccache --test compiler_response_file_expansion_test -- <test_name> --nocapture

use std::path::Path;
use zccache::compiler::response_file::{
    expand_response_files, parse_response_file_content, ResponseFileError,
};
use zccache::core::NormalizedPath;

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 6: EXPANSION — DEPTH BOUNDARY TESTS
// ═══════════════════════════════════════════════════════════════════════════════

/// Helper: create a chain of N response files, each referencing the next.
/// The deepest file contains `-DLEAF`.
/// Returns the path to the outermost file.
fn create_nested_chain(dir: &Path, depth: usize) -> NormalizedPath {
    // Create files from deepest to shallowest
    let deepest = dir.join(format!("level_{depth}.rsp"));
    std::fs::write(&deepest, "-DLEAF").unwrap();

    let mut prev_path = NormalizedPath::new(deepest);
    for i in (0..depth).rev() {
        let this_path = dir.join(format!("level_{i}.rsp"));
        let content = format!("-DLEVEL_{i} @{}", prev_path.display());
        std::fs::write(&this_path, content).unwrap();
        prev_path = NormalizedPath::new(this_path);
    }
    prev_path
}

/// Exactly 10 levels of nesting (MAX_DEPTH=10). Should succeed.
/// Level 0 → level 1 → ... → level 9 → level 10 (leaf, no @ref).
/// depth parameter to expand_recursive goes: 0, 1, 2, ..., 9.
/// At depth 9, it reads level_9.rsp which has @level_10.rsp.
/// depth becomes 10, which is >= MAX_DEPTH → TooDeep.
/// So 10 levels of nesting actually fails! Let's verify the boundary.
#[test]
fn expand_depth_exactly_at_max() {
    let dir = tempfile::tempdir().unwrap();
    // 9 levels of nesting: level_0 → level_1 → ... → level_8 → level_9 (leaf)
    let root = create_nested_chain(dir.path(), 9);
    let args = s(&[&format!("@{}", root.display())]);
    let result = expand_response_files(&args);
    assert!(
        result.is_ok(),
        "9 levels of nesting should succeed: {result:?}"
    );
    let expanded = result.unwrap();
    assert!(expanded.contains(&"-DLEAF".to_string()));
    assert!(expanded.contains(&"-DLEVEL_0".to_string()));
}

/// 10 levels of nesting: should hit MAX_DEPTH and fail.
#[test]
fn expand_depth_one_past_max() {
    let dir = tempfile::tempdir().unwrap();
    let root = create_nested_chain(dir.path(), 10);
    let args = s(&[&format!("@{}", root.display())]);
    let result = expand_response_files(&args);
    assert!(result.is_err(), "10 levels should exceed MAX_DEPTH");
    assert!(
        matches!(result.unwrap_err(), ResponseFileError::TooDeep { .. }),
        "expected TooDeep error"
    );
}

/// Exactly MAX_DEPTH-1 levels: should succeed.
#[test]
fn expand_depth_one_below_max() {
    let dir = tempfile::tempdir().unwrap();
    let root = create_nested_chain(dir.path(), 8);
    let args = s(&[&format!("@{}", root.display())]);
    let result = expand_response_files(&args);
    assert!(result.is_ok(), "8 levels should succeed: {result:?}");
}

/// Wide fan at each level (each file has 3 @refs to children). Depth 3, fan 3 → 40 files.
#[test]
fn expand_wide_fan_nested() {
    let dir = tempfile::tempdir().unwrap();

    // Create leaf files
    for i in 0..9 {
        let path = dir.path().join(format!("leaf_{i}.rsp"));
        std::fs::write(&path, format!("-DLEAF_{i}")).unwrap();
    }

    // Create mid-level files, each referencing 3 leaves
    for i in 0..3 {
        let path = dir.path().join(format!("mid_{i}.rsp"));
        let mut content = format!("-DMID_{i}");
        for j in 0..3 {
            let leaf = dir.path().join(format!("leaf_{}.rsp", i * 3 + j));
            content.push_str(&format!(" @{}", leaf.display()));
        }
        std::fs::write(&path, content).unwrap();
    }

    // Create root referencing all mid files
    let root = dir.path().join("root.rsp");
    let mut content = "-DROOT".to_string();
    for i in 0..3 {
        let mid = dir.path().join(format!("mid_{i}.rsp"));
        content.push_str(&format!(" @{}", mid.display()));
    }
    std::fs::write(&root, content).unwrap();

    let args = s(&[&format!("@{}", root.display())]);
    let result = expand_response_files(&args).unwrap();

    // Should have: DROOT + 3×DMID_x + 9×DLEAF_x = 13 args
    assert_eq!(result.len(), 13);
    assert!(result.contains(&"-DROOT".to_string()));
    for i in 0..3 {
        assert!(result.contains(&format!("-DMID_{i}")));
    }
    for i in 0..9 {
        assert!(result.contains(&format!("-DLEAF_{i}")));
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 7: EXPANSION — CIRCULAR REFERENCE VARIANTS
// ═══════════════════════════════════════════════════════════════════════════════

/// Triangle cycle: A → B → C → A.
#[test]
fn expand_triangle_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let path_a = dir.path().join("a.rsp");
    let path_b = dir.path().join("b.rsp");
    let path_c = dir.path().join("c.rsp");

    std::fs::write(&path_a, format!("@{}", path_b.display())).unwrap();
    std::fs::write(&path_b, format!("@{}", path_c.display())).unwrap();
    std::fs::write(&path_c, format!("@{}", path_a.display())).unwrap();

    let args = s(&[&format!("@{}", path_a.display())]);
    let result = expand_response_files(&args);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        ResponseFileError::CircularReference { .. }
    ));
}

/// Cycle not involving root: A → B → C → D → B (cycle at B, not A).
#[test]
fn expand_deep_cycle_not_at_root() {
    let dir = tempfile::tempdir().unwrap();
    let path_a = dir.path().join("a.rsp");
    let path_b = dir.path().join("b.rsp");
    let path_c = dir.path().join("c.rsp");
    let path_d = dir.path().join("d.rsp");

    std::fs::write(&path_a, format!("@{}", path_b.display())).unwrap();
    std::fs::write(&path_b, format!("@{}", path_c.display())).unwrap();
    std::fs::write(&path_c, format!("@{}", path_d.display())).unwrap();
    std::fs::write(&path_d, format!("@{}", path_b.display())).unwrap();

    let args = s(&[&format!("@{}", path_a.display())]);
    let result = expand_response_files(&args);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        ResponseFileError::CircularReference { .. }
    ));
}

/// Diamond: A → {B, C}, B → D, C → D. NOT circular (D appears in siblings).
#[test]
fn expand_diamond_not_circular() {
    let dir = tempfile::tempdir().unwrap();
    let path_d = dir.path().join("d.rsp");
    let path_b = dir.path().join("b.rsp");
    let path_c = dir.path().join("c.rsp");
    let path_a = dir.path().join("a.rsp");

    std::fs::write(&path_d, "-DFROM_D").unwrap();
    std::fs::write(&path_b, format!("-DFROM_B @{}", path_d.display())).unwrap();
    std::fs::write(&path_c, format!("-DFROM_C @{}", path_d.display())).unwrap();
    std::fs::write(
        &path_a,
        format!("@{} @{}", path_b.display(), path_c.display()),
    )
    .unwrap();

    let args = s(&[&format!("@{}", path_a.display())]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["-DFROM_B", "-DFROM_D", "-DFROM_C", "-DFROM_D"]));
}

/// Self-reference with other content: file has args AND @self.
#[test]
fn expand_self_reference_with_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("self.rsp");
    std::fs::write(&path, format!("-O2 -Wall @{}", path.display())).unwrap();

    let args = s(&[&format!("@{}", path.display())]);
    let result = expand_response_files(&args);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        ResponseFileError::CircularReference { .. }
    ));
}

/// Same file referenced 3 times at top level (siblings) → should all expand.
#[test]
fn expand_same_file_three_siblings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shared.rsp");
    std::fs::write(&path, "-DSHARED").unwrap();

    let ref_str = format!("@{}", path.display());
    let args = s(&[&ref_str, &ref_str, &ref_str]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["-DSHARED", "-DSHARED", "-DSHARED"]));
}

/// A references B, then later references B again (sibling positions). Should work.
#[test]
fn expand_same_file_referenced_twice_in_parent() {
    let dir = tempfile::tempdir().unwrap();
    let path_b = dir.path().join("b.rsp");
    let path_a = dir.path().join("a.rsp");

    std::fs::write(&path_b, "-DFROM_B").unwrap();
    std::fs::write(
        &path_a,
        format!("@{} -DMIDDLE @{}", path_b.display(), path_b.display()),
    )
    .unwrap();

    let args = s(&[&format!("@{}", path_a.display())]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["-DFROM_B", "-DMIDDLE", "-DFROM_B"]));
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 8: EXPANSION — FILE CONTENT EDGE CASES
// ═══════════════════════════════════════════════════════════════════════════════

/// Empty response file → no args contributed.
#[test]
fn expand_empty_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.rsp");
    std::fs::write(&path, "").unwrap();

    let args = s(&["-O2", &format!("@{}", path.display()), "-Wall"]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["-O2", "-Wall"]));
}

/// Whitespace-only response file → no args contributed.
#[test]
fn expand_whitespace_only_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ws.rsp");
    std::fs::write(&path, "   \n\t\r\n  ").unwrap();

    let args = s(&["-O2", &format!("@{}", path.display())]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["-O2"]));
}

/// Response file with UTF-8 BOM.
#[test]
fn expand_file_with_bom() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bom.rsp");
    std::fs::write(&path, "\u{FEFF}-O2 -Wall").unwrap();

    let args = s(&[&format!("@{}", path.display())]);
    let result = expand_response_files(&args).unwrap();
    // BOM becomes part of first arg
    assert_eq!(result.len(), 2);
    assert!(result[0].ends_with("-O2"));
    assert_eq!(result[1], "-Wall");
}

/// Response file with CRLF line endings.
#[test]
fn expand_file_with_crlf() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crlf.rsp");
    std::fs::write(&path, "-O2\r\n-Wall\r\n-DNDEBUG\r\n").unwrap();

    let args = s(&[&format!("@{}", path.display())]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["-O2", "-Wall", "-DNDEBUG"]));
}

/// Response file that produces args starting with @.
/// These should be recursively expanded.
#[test]
fn expand_file_producing_at_args() {
    let dir = tempfile::tempdir().unwrap();
    let inner = dir.path().join("inner.rsp");
    std::fs::write(&inner, "-DINNER").unwrap();

    let outer = dir.path().join("outer.rsp");
    std::fs::write(&outer, format!("-DOUTER @{}", inner.display())).unwrap();

    let args = s(&[&format!("@{}", outer.display())]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["-DOUTER", "-DINNER"]));
}

/// Response file that contains a bare @ — should pass through.
#[test]
fn expand_file_with_bare_at() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bare_at.rsp");
    std::fs::write(&path, "-O2 @ -Wall").unwrap();

    let args = s(&[&format!("@{}", path.display())]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["-O2", "@", "-Wall"]));
}

/// Response file with double-@ prefix: @@file → @file is the filename.
#[test]
fn expand_double_at_prefix() {
    let dir = tempfile::tempdir().unwrap();
    // Create a file named "@oddname.rsp"
    let path = dir.path().join("@oddname.rsp");
    std::fs::write(&path, "-DODD").unwrap();

    // @@oddname.rsp → strip one @, look for file "@oddname.rsp"
    let args = s(&[&format!("@@{}", dir.path().join("oddname.rsp").display())]);
    // This will try to find file "@<dir>/oddname.rsp" which doesn't exist at that path
    // Actually, strip_prefix('@') gives "@<dir>/oddname.rsp"
    // so it looks for file named "@<dir>/oddname.rsp" which doesn't exist.
    // This should be a ReadError.
    let result = expand_response_files(&args);
    assert!(matches!(result, Err(ResponseFileError::ReadError { .. })));
}

/// Non-existent file gives ReadError.
#[test]
fn expand_nonexistent_file() {
    let args = s(&["@/this/path/surely/does/not/exist.rsp"]);
    let result = expand_response_files(&args);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        ResponseFileError::ReadError { .. }
    ));
}

/// Response file path with spaces.
#[test]
fn expand_path_with_spaces() {
    let dir = tempfile::tempdir().unwrap();
    let subdir = dir.path().join("path with spaces");
    std::fs::create_dir_all(&subdir).unwrap();
    let path = subdir.join("args.rsp");
    std::fs::write(&path, "-O2 -Wall").unwrap();

    let args = s(&[&format!("@{}", path.display())]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["-O2", "-Wall"]));
}

/// Mix of inline args and response file args — order preserved correctly.
#[test]
fn expand_interleaved_inline_and_file_args() {
    let dir = tempfile::tempdir().unwrap();
    let f1 = dir.path().join("f1.rsp");
    let f2 = dir.path().join("f2.rsp");
    std::fs::write(&f1, "-B -C").unwrap();
    std::fs::write(&f2, "-F -G").unwrap();

    let args = s(&[
        "-A",
        &format!("@{}", f1.display()),
        "-D",
        "-E",
        &format!("@{}", f2.display()),
        "-H",
    ]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["-A", "-B", "-C", "-D", "-E", "-F", "-G", "-H"]));
}

/// Large response file: 10,000 arguments.
#[test]
fn expand_large_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.rsp");
    let content: String = (0..10_000)
        .map(|i| format!("-D_{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, content).unwrap();

    let args = s(&[&format!("@{}", path.display())]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result.len(), 10_000);
    assert_eq!(result[0], "-D_0");
    assert_eq!(result[9999], "-D_9999");
}

/// Response file with quoted content containing the @ character.
#[test]
fn expand_file_with_quoted_at() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("quoted_at.rsp");
    // Quoted @file should NOT be expanded — it's an argument value.
    // But our parser doesn't distinguish: it just parses args.
    // "@nonexistent" becomes @nonexistent (without quotes) and WILL be expanded.
    // This tests that behavior.
    std::fs::write(&path, "-DFOO \"@not_a_file\"").unwrap();

    // "@not_a_file" after parsing becomes @not_a_file, which the expander
    // will try to treat as a response file reference.
    let args = s(&[&format!("@{}", path.display())]);
    let result = expand_response_files(&args);
    // Should fail because @not_a_file references a nonexistent file
    assert!(matches!(result, Err(ResponseFileError::ReadError { .. })));
}

/// Response file with all content on one line.
#[test]
fn expand_file_single_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oneline.rsp");
    std::fs::write(&path, "-c foo.cpp -o foo.o -O2 -Wall -DNDEBUG -std=c++17").unwrap();

    let args = s(&[&format!("@{}", path.display())]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(
        result,
        s(&[
            "-c",
            "foo.cpp",
            "-o",
            "foo.o",
            "-O2",
            "-Wall",
            "-DNDEBUG",
            "-std=c++17"
        ])
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 10: STRESS — PERFORMANCE AND SCALE
// ═══════════════════════════════════════════════════════════════════════════════

/// Parse a 1MB response file content string.
#[test]
fn stress_parse_1mb_content() {
    // 1MB of -DFOO_XXXXX args
    let mut content = String::with_capacity(1_100_000);
    let mut count = 0;
    while content.len() < 1_000_000 {
        content.push_str(&format!("-DFOO_{count} "));
        count += 1;
    }
    let result = parse_response_file_content(&content);
    assert_eq!(result.len(), count);
}

/// Expand a chain where each file produces many args + one nested ref.
#[test]
fn stress_expand_chain_with_many_args() {
    let dir = tempfile::tempdir().unwrap();
    let depth = 5;

    let leaf = dir.path().join("leaf.rsp");
    let leaf_content: String = (0..100)
        .map(|i| format!("-DLEAF_{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    std::fs::write(&leaf, leaf_content).unwrap();

    let mut prev = leaf;
    for i in 0..depth {
        let this = dir.path().join(format!("level_{i}.rsp"));
        let args: String = (0..100)
            .map(|j| format!("-DL{i}_{j}"))
            .collect::<Vec<_>>()
            .join(" ");
        std::fs::write(&this, format!("{args} @{}", prev.display())).unwrap();
        prev = this;
    }

    let args = s(&[&format!("@{}", prev.display())]);
    let result = expand_response_files(&args).unwrap();
    // 5 levels × 100 args + 100 leaf args = 600
    assert_eq!(result.len(), (depth + 1) * 100);
}

/// Many sibling response file references (100 files).
#[test]
fn stress_expand_many_siblings() {
    let dir = tempfile::tempdir().unwrap();
    let mut arg_strs = Vec::new();

    for i in 0..100 {
        let path = dir.path().join(format!("sibling_{i}.rsp"));
        std::fs::write(&path, format!("-DSIB_{i}")).unwrap();
        arg_strs.push(format!("@{}", path.display()));
    }

    let args: Vec<String> = arg_strs;
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result.len(), 100);
    for (i, arg) in result.iter().enumerate() {
        assert_eq!(arg, &format!("-DSIB_{i}"));
    }
}

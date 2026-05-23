//! Integration tests for depfile parsing with real compilers.
//!
//! These tests require GCC or Clang to be installed. Run with:
//! `soldr cargo test -p zccache-depgraph --test depfile_integration_test -- --ignored`

use std::process::Command;
use tempfile::TempDir;
use zccache_monocrate::core::NormalizedPath;

/// Check if a compiler is available on PATH.
fn has_compiler(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Find available C compiler: prefer gcc, fall back to clang, then cc.
fn find_cc() -> Option<String> {
    for name in &["gcc", "clang", "cc"] {
        if has_compiler(name) {
            return Some(name.to_string());
        }
    }
    None
}

#[test]
#[ignore]
fn depfile_basic() {
    let cc = match find_cc() {
        Some(c) => c,
        None => {
            eprintln!("skipping: no C compiler found");
            return;
        }
    };

    let dir = TempDir::new().unwrap();
    let cwd = dir.path();

    // Create source and header
    std::fs::write(
        cwd.join("main.c"),
        r#"
        #include "util.h"
        int main() { return helper(); }
    "#,
    )
    .unwrap();
    std::fs::write(
        cwd.join("util.h"),
        r#"
        int helper(void);
    "#,
    )
    .unwrap();

    let depfile = cwd.join("main.d");

    // Compile with -MD -MF
    let output = Command::new(&cc)
        .args(["-c", "main.c", "-o", "main.o", "-MD", "-MF"])
        .arg(depfile.to_str().unwrap())
        .current_dir(cwd)
        .output()
        .expect("failed to run compiler");

    assert!(
        output.status.success(),
        "compiler failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(depfile.exists(), "depfile not created");

    // Parse depfile
    let source = cwd.join("main.c");
    let result = zccache_depgraph::depfile::parse_depfile_path(&depfile, &source, cwd).unwrap();

    // Should contain util.h (and possibly system headers)
    let util_h = std::fs::canonicalize(cwd.join("util.h")).unwrap();
    assert!(
        result.resolved.contains(&NormalizedPath::new(&util_h)),
        "expected util.h in resolved deps: {:?}",
        result.resolved
    );
    assert!(!result.has_computed);
    assert!(result.unresolved.is_empty());
}

#[test]
#[ignore]
fn depfile_nested_includes() {
    let cc = match find_cc() {
        Some(c) => c,
        None => return,
    };

    let dir = TempDir::new().unwrap();
    let cwd = dir.path();

    std::fs::write(
        cwd.join("main.c"),
        r#"#include "a.h"
int main() { return 0; }
"#,
    )
    .unwrap();
    std::fs::write(
        cwd.join("a.h"),
        r#"#include "b.h"
"#,
    )
    .unwrap();
    std::fs::write(
        cwd.join("b.h"),
        r#"#include "c.h"
"#,
    )
    .unwrap();
    std::fs::write(
        cwd.join("c.h"),
        r#"/* leaf */
"#,
    )
    .unwrap();

    let depfile = cwd.join("main.d");

    let output = Command::new(&cc)
        .args(["-c", "main.c", "-o", "main.o", "-MD", "-MF"])
        .arg(depfile.to_str().unwrap())
        .current_dir(cwd)
        .output()
        .expect("failed to run compiler");

    assert!(output.status.success());

    let source = cwd.join("main.c");
    let result = zccache_depgraph::depfile::parse_depfile_path(&depfile, &source, cwd).unwrap();

    // All three headers should be in the dependency list
    let a_h = std::fs::canonicalize(cwd.join("a.h")).unwrap();
    let b_h = std::fs::canonicalize(cwd.join("b.h")).unwrap();
    let c_h = std::fs::canonicalize(cwd.join("c.h")).unwrap();

    assert!(
        result.resolved.contains(&NormalizedPath::new(&a_h)),
        "missing a.h: {:?}",
        result.resolved
    );
    assert!(
        result.resolved.contains(&NormalizedPath::new(&b_h)),
        "missing b.h: {:?}",
        result.resolved
    );
    assert!(
        result.resolved.contains(&NormalizedPath::new(&c_h)),
        "missing c.h: {:?}",
        result.resolved
    );
}

#[test]
#[ignore]
fn depfile_computed_include() {
    let cc = match find_cc() {
        Some(c) => c,
        None => return,
    };

    let dir = TempDir::new().unwrap();
    let cwd = dir.path();

    // Source uses a computed include via macro
    std::fs::write(
        cwd.join("main.c"),
        r#"
        #define MY_HEADER "computed.h"
        #include MY_HEADER
        int main() { return val(); }
    "#,
    )
    .unwrap();
    std::fs::write(
        cwd.join("computed.h"),
        r#"int val(void);
"#,
    )
    .unwrap();

    let depfile = cwd.join("main.d");

    let output = Command::new(&cc)
        .args(["-c", "main.c", "-o", "main.o", "-MD", "-MF"])
        .arg(depfile.to_str().unwrap())
        .current_dir(cwd)
        .output()
        .expect("failed to run compiler");

    assert!(
        output.status.success(),
        "compiler failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let source = cwd.join("main.c");
    let result = zccache_depgraph::depfile::parse_depfile_path(&depfile, &source, cwd).unwrap();

    // The compiler resolves computed includes — depfile should contain computed.h
    let computed_h = std::fs::canonicalize(cwd.join("computed.h")).unwrap();
    assert!(
        result.resolved.contains(&NormalizedPath::new(&computed_h)),
        "depfile should resolve computed includes: {:?}",
        result.resolved
    );
    // has_computed is false because depfiles don't have computed includes
    assert!(!result.has_computed);
}

#[test]
#[ignore]
fn depfile_system_headers_with_md() {
    let cc = match find_cc() {
        Some(c) => c,
        None => return,
    };

    let dir = TempDir::new().unwrap();
    let cwd = dir.path();

    std::fs::write(
        cwd.join("main.c"),
        r#"
        #include <stdio.h>
        int main() { return 0; }
    "#,
    )
    .unwrap();

    let depfile = cwd.join("main.d");

    // -MD (not -MMD) should include system headers
    let output = Command::new(&cc)
        .args(["-c", "main.c", "-o", "main.o", "-MD", "-MF"])
        .arg(depfile.to_str().unwrap())
        .current_dir(cwd)
        .output()
        .expect("failed to run compiler");

    assert!(output.status.success());

    let source = cwd.join("main.c");
    let result = zccache_depgraph::depfile::parse_depfile_path(&depfile, &source, cwd).unwrap();

    // Should contain at least one system header (stdio.h or its internal includes)
    assert!(
        !result.resolved.is_empty(),
        "expected system headers in -MD output"
    );
}

#[test]
#[ignore]
fn depfile_parity_with_scanner() {
    // Verify that depfile parsing finds the same user headers as scan_recursive
    let cc = match find_cc() {
        Some(c) => c,
        None => return,
    };

    let dir = TempDir::new().unwrap();
    let cwd = dir.path();

    std::fs::write(
        cwd.join("main.c"),
        r#"
        #include "alpha.h"
        #include "beta.h"
        int main() { return 0; }
    "#,
    )
    .unwrap();
    std::fs::write(
        cwd.join("alpha.h"),
        r#"#include "gamma.h"
"#,
    )
    .unwrap();
    std::fs::write(
        cwd.join("beta.h"),
        r#"/* no includes */
"#,
    )
    .unwrap();
    std::fs::write(
        cwd.join("gamma.h"),
        r#"/* leaf */
"#,
    )
    .unwrap();

    let depfile_path = cwd.join("main.d");

    // Compile with -MMD (user headers only, for fair comparison)
    let output = Command::new(&cc)
        .args(["-c", "main.c", "-o", "main.o", "-MMD", "-MF"])
        .arg(depfile_path.to_str().unwrap())
        .current_dir(cwd)
        .output()
        .expect("failed to run compiler");

    assert!(output.status.success());

    let source = cwd.join("main.c");

    // Parse depfile
    let depfile_result =
        zccache_depgraph::depfile::parse_depfile_path(&depfile_path, &source, cwd).unwrap();

    // Scan with scanner
    let search_paths = zccache_depgraph::IncludeSearchPaths {
        iquote: vec![],
        user: vec![cwd.to_path_buf().into()],
        system: vec![],
        after: vec![],
    };
    let scan_result = zccache_depgraph::scanner::scan_recursive(&source, &search_paths);

    // All scanner-resolved paths should appear in depfile results
    for path in &scan_result.resolved {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        assert!(
            depfile_result
                .resolved
                .contains(&NormalizedPath::new(&canonical)),
            "depfile missing scanner-found header: {}",
            path.display()
        );
    }
}

#[test]
#[ignore]
fn depfile_strategy_injected_works_end_to_end() {
    // Test the full prepare_depfile → compile → parse pipeline
    let cc = match find_cc() {
        Some(c) => c,
        None => return,
    };

    let dir = TempDir::new().unwrap();
    let cwd = dir.path();
    let tmpdir = cwd.join("tmp");
    std::fs::create_dir(&tmpdir).unwrap();

    std::fs::write(
        cwd.join("test.c"),
        r#"
        #include "test.h"
        int main() { return 0; }
    "#,
    )
    .unwrap();
    std::fs::write(cwd.join("test.h"), "/* header */\n").unwrap();

    let output_file = cwd.join("test.o");

    let dep_flags = zccache_depgraph::UserDepFlags::default();
    let (extra_args, strategy) =
        zccache_depgraph::depfile::prepare_depfile(true, &dep_flags, &output_file, &tmpdir);

    let depfile_path = match &strategy {
        zccache_depgraph::DepfileStrategy::Injected { path } => path.clone(),
        other => panic!("expected Injected, got: {other:?}"),
    };

    // Compile with injected args
    let mut args = vec![
        "-c".to_string(),
        "test.c".to_string(),
        "-o".to_string(),
        output_file.to_string_lossy().into_owned(),
    ];
    args.extend(extra_args);

    let output = Command::new(&cc)
        .args(&args)
        .current_dir(cwd)
        .output()
        .expect("failed to run compiler");

    assert!(
        output.status.success(),
        "compiler failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        depfile_path.exists(),
        "depfile not created at {:?}",
        depfile_path
    );

    // Parse depfile
    let source = cwd.join("test.c");
    let result =
        zccache_depgraph::depfile::parse_depfile_path(&depfile_path, &source, cwd).unwrap();

    let test_h = std::fs::canonicalize(cwd.join("test.h")).unwrap();
    assert!(
        result.resolved.contains(&NormalizedPath::new(&test_h)),
        "expected test.h in deps: {:?}",
        result.resolved
    );

    // Clean up injected depfile
    std::fs::remove_file(&depfile_path).unwrap();
    assert!(!depfile_path.exists());
}

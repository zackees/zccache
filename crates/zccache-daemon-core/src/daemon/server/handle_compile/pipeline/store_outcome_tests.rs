use crate::core::NormalizedPath;
use crate::daemon::server::dependency_policy::DependencyDiscoveryMode;

#[test]
fn mmd_fallback_tracks_user_headers_but_omits_system_roots() {
    let temp = tempfile::tempdir().unwrap();
    let user_dir = temp.path().join("user");
    let system_dir = temp.path().join("system");
    std::fs::create_dir_all(&user_dir).unwrap();
    std::fs::create_dir_all(&system_dir).unwrap();
    let source = temp.path().join("main.cpp");
    let user_header = user_dir.join("user.hpp");
    let system_header = system_dir.join("system.hpp");
    std::fs::write(&source, "#include <user.hpp>\n#include <system.hpp>\n").unwrap();
    std::fs::write(&user_header, "#define USER 1\n").unwrap();
    std::fs::write(&system_header, "#define SYSTEM 1\n").unwrap();
    let source: NormalizedPath = std::fs::canonicalize(source).unwrap().into();
    let user_header: NormalizedPath = std::fs::canonicalize(user_header).unwrap().into();
    let system_header: NormalizedPath = std::fs::canonicalize(system_header).unwrap().into();
    let search = crate::depgraph::IncludeSearchPaths {
        user: vec![user_dir.into()],
        system: vec![system_dir.into()],
        ..Default::default()
    }
    .canonicalized();

    let mut result = crate::depgraph::scanner::scan_recursive(&source, &search);
    super::scan::apply_static_fallback_policy(
        DependencyDiscoveryMode::SkipSystemHeaders,
        true,
        &mut result,
        &search,
    );

    assert!(result.resolved.contains(&user_header));
    assert!(!result.resolved.contains(&system_header));
    assert!(result.unresolved.is_empty());
    assert!(result.has_computed);
}

#[test]
fn compiler_manifest_keeps_user_header_beneath_system_root() {
    let search = crate::depgraph::IncludeSearchPaths {
        system: vec!["/sdk".into()],
        ..Default::default()
    };
    let sibling: NormalizedPath = "/sdk/project/config.hpp".into();
    let mut result = crate::depgraph::ScanResult {
        resolved: vec![sibling.clone()],
        unresolved: Vec::new(),
        has_computed: false,
    };

    super::scan::apply_static_fallback_policy(
        DependencyDiscoveryMode::SkipSystemHeaders,
        false,
        &mut result,
        &search,
    );

    assert_eq!(result.resolved, vec![sibling]);
    assert!(!result.has_computed);
}

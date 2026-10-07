//! #1935: queued watcher events cannot authorize an old compiler artifact.

use super::super::*;

#[tokio::test]
async fn fast_entry_checks_source_headers_force_includes_and_externs_without_events() {
    for changed in [
        "main.rs",
        "inc/header.h",
        "inc/force.h",
        "deps/external.rlib",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let server = super::bind_isolated_server(dir.path());
        let names = [
            "main.rs",
            "inc/header.h",
            "inc/force.h",
            "deps/external.rlib",
        ];
        let paths: Vec<NormalizedPath> = names.iter().map(|p| dir.path().join(p).into()).collect();
        for path in &paths {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"old").unwrap();
            server.state.cache_system.journal().register(path.clone());
            server
                .state
                .cache_system
                .lookup_since(path, Clock::ZERO)
                .unwrap();
        }
        let ctx = CompileContext {
            source_file: paths[0].clone(),
            include_search: crate::depgraph::IncludeSearchPaths::default(),
            defines: vec![],
            undefines: vec![],
            flags: vec![],
            force_includes: vec![paths[2].clone()],
            unknown_flags: vec![],
            compiler_hash: crate::hash::hash_bytes(b"fixture"),
        };
        let graph = server.state.dep_graph.load();
        let registration = graph.register_rustc_with_key_and_root_result(
            ctx.context_key(),
            ctx,
            None,
            vec![("dep".into(), paths[3].clone())],
            None,
        );
        let scan = crate::depgraph::ScanResult {
            resolved: vec![paths[1].clone()],
            unresolved: vec![],
            has_computed: false,
        };
        let hash = |path: &Path| {
            std::fs::read(path)
                .ok()
                .map(|b| crate::hash::hash_bytes(&b))
        };
        let expected = graph
            .update(&registration.map_key, scan.clone(), hash)
            .unwrap()
            .hash()
            .to_hex();
        let clock = server.state.cache_system.current_clock();
        let fresh = || {
            context_artifact_is_fresh(
                &server.state,
                &registration.map_key,
                &paths[0],
                clock,
                None,
                &expected,
            )
        };
        assert!(fresh(), "unchanged: {changed}");
        let edited: NormalizedPath = dir.path().join(changed).into();
        std::fs::write(&edited, b"new").unwrap();
        assert!(!server
            .state
            .cache_system
            .journal()
            .changed_since(&edited, clock));
        assert!(!fresh(), "pending event: {changed}");
        // Even another request's updated metadata cannot prove the old key.
        server
            .state
            .cache_system
            .lookup_since(&edited, Clock::ZERO)
            .unwrap();
        assert!(!fresh(), "updated metadata: {changed}");
        // A newer depgraph key must not authorize an older fast-entry key.
        graph.update(&registration.map_key, scan, hash).unwrap();
        assert!(!fresh(), "updated context: {changed}");
    }
}

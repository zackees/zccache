//! Ignored watcher paths must not authorize journal-only cache hits.

use super::super::*;

#[tokio::test]
async fn ignored_source_requires_validation_without_a_watcher_event() {
    let dir = tempfile::tempdir().unwrap();
    let server = super::bind_isolated_server(dir.path());
    let source = dir.path().join(".cache/probe/src/lib.rs");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, "pub fn value() -> u32 { 17 }\n").unwrap();
    let key = ContextKey::from_raw([1; 32]);
    server
        .state
        .cache_system
        .journal()
        .register(source.clone().into());
    let clock = server.state.cache_system.journal().current_clock();
    std::fs::write(&source, "pub fn value() -> u32 { 29 }\n").unwrap();
    assert!(!context_files_fresh(&server.state, &key, &source, clock));
}

#[tokio::test]
async fn ignored_include_requires_validation_for_an_ordinary_source() {
    let dir = tempfile::tempdir().unwrap();
    let server = super::bind_isolated_server(dir.path());
    let source = dir.path().join("main.c");
    let header = dir.path().join("build/generated.h");
    std::fs::create_dir_all(header.parent().unwrap()).unwrap();
    std::fs::write(&source, "#include \"build/generated.h\"\n").unwrap();
    std::fs::write(&header, "#define VALUE 17\n").unwrap();
    let graph = server.state.dep_graph.load();
    let key = graph.register(CompileContext {
        source_file: source.clone().into(),
        include_search: crate::depgraph::IncludeSearchPaths::default(),
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
        compiler_hash: crate::hash::hash_bytes(b"fixture"),
    });
    graph
        .update(
            &key,
            crate::depgraph::ScanResult {
                resolved: vec![header.clone().into()],
                unresolved: Vec::new(),
                has_computed: false,
            },
            |path| {
                std::fs::read(path)
                    .ok()
                    .map(|bytes| crate::hash::hash_bytes(&bytes))
            },
        )
        .unwrap();
    server
        .state
        .cache_system
        .journal()
        .register(source.clone().into());
    server
        .state
        .cache_system
        .journal()
        .register(header.clone().into());
    let clock = server.state.cache_system.journal().current_clock();
    std::fs::write(&header, "#define VALUE 29\n").unwrap();
    assert!(!context_files_fresh(&server.state, &key, &source, clock));
}

#[tokio::test]
async fn ordinary_source_retains_journal_fast_path() {
    let dir = tempfile::tempdir().unwrap();
    let server = super::bind_isolated_server(dir.path());
    let source = dir.path().join("main.c");
    std::fs::write(&source, "int value = 17;\n").unwrap();
    let key = ContextKey::from_raw([2; 32]);
    server
        .state
        .cache_system
        .journal()
        .register(source.clone().into());
    let clock = server.state.cache_system.journal().current_clock();
    assert!(context_files_fresh(&server.state, &key, &source, clock));
}

#[tokio::test]
async fn ignored_extern_requires_validation_for_an_ordinary_source() {
    let dir = tempfile::tempdir().unwrap();
    let server = super::bind_isolated_server(dir.path());
    let source = dir.path().join("main.rs");
    let dependency = dir.path().join("target/libdep.rlib");
    std::fs::create_dir_all(dependency.parent().unwrap()).unwrap();
    std::fs::write(&source, "fn main() {}\n").unwrap();
    std::fs::write(&dependency, "old dependency").unwrap();
    let ctx = CompileContext {
        source_file: source.clone().into(),
        include_search: crate::depgraph::IncludeSearchPaths::default(),
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
        compiler_hash: crate::hash::hash_bytes(b"fixture"),
    };
    let registration = server
        .state
        .dep_graph
        .load()
        .register_rustc_with_key_and_root_result(
            ctx.context_key(),
            ctx,
            None,
            vec![("dep".into(), dependency.clone().into())],
            None,
        );
    let journal = server.state.cache_system.journal();
    journal.register(source.clone().into());
    journal.register(dependency.clone().into());
    let clock = journal.current_clock();
    std::fs::write(&dependency, "new dependency").unwrap();
    assert!(!context_files_fresh(
        &server.state,
        &registration.map_key,
        &source,
        clock
    ));
}

#[test]
fn ignored_cross_root_inputs_require_hash_validation() {
    let dir = tempfile::tempdir().unwrap();
    let input: NormalizedPath = dir.path().join(".cache/lib.rs").into();
    let journal = crate::fscache::ChangeJournal::new();
    journal.register(input.clone());
    let clock = journal.current_clock();
    assert!(!request_cache_inputs_fresh_since(&journal, &[input], clock));
}

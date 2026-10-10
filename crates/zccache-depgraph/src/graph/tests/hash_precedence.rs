//! #1935: a measured content change overrides watcher journal silence.

use super::*;
use crate::{CompileContext, IncludeSearchPaths, ScanResult};

#[test]
fn fresh_hash_overrides_journal_for_every_input_and_verdict_path() {
    for changed in [
        "/src/main.rs",
        "/inc/header.h",
        "/inc/force.h",
        "/dep/lib.rlib",
    ] {
        for mode in ["plain", "diagnostic", "metadata"] {
            let graph = DepGraph::new();
            let ctx = CompileContext {
                source_file: "/src/main.rs".into(),
                include_search: IncludeSearchPaths::default(),
                defines: vec![],
                undefines: vec![],
                flags: vec![],
                force_includes: vec!["/inc/force.h".into()],
                unknown_flags: vec![],
                compiler_hash: zccache_hash::hash_bytes(b"compiler"),
            };
            let registration = graph.register_rustc_with_key_and_root_result(
                ctx.context_key(),
                ctx,
                None,
                vec![("dep".into(), "/dep/lib.rlib".into())],
                Some(ContextKey::from_raw([3; 32])),
            );
            let key = registration.map_key;
            let original =
                |path: &Path| Some(zccache_hash::hash_bytes(path.to_string_lossy().as_bytes()));
            graph
                .update(
                    &key,
                    ScanResult {
                        resolved: vec!["/inc/header.h".into()],
                        unresolved: vec![],
                        has_computed: false,
                    },
                    original,
                )
                .unwrap();
            let current = |path: &Path| {
                if path == Path::new(changed) {
                    Some(zccache_hash::hash_bytes(b"changed"))
                } else {
                    original(path)
                }
            };
            let verdict = match mode {
                "plain" => graph.check(&key, |_| true, current),
                "diagnostic" => graph.check_diagnostic(&key, |_| true, current).0,
                _ => {
                    graph
                        .check_rustc_metadata_compat_diagnostic(
                            &registration.metadata_compat_map_key.unwrap(),
                            &[("dep".into(), "/dep/lib.rlib".into())],
                            |_| true,
                            current,
                        )
                        .0
                }
            };
            assert!(
                !matches!(verdict, CacheVerdict::Hit { .. }),
                "{mode}: {changed}: {verdict:?}"
            );
        }
    }
}

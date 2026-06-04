# Task: Restore depfile on cache hit (#643)

## Plan

- [x] Read CLAUDE.md / docs hierarchy
- [x] Read `depgraph/args.rs`, `depgraph/depfile.rs`, `handle_compile/{pipeline,miss_store,cached_hit,hit_branches}.rs`
- [x] Map call sites for `materialize_cached_compile_hit`
- [ ] **RED**: write failing inline test in `cached_hit.rs` proving 2-output artifact + `current_depfile_dest` writes both files
- [ ] **GREEN**: add `current_depfile_dest: Option<NormalizedPath>` to `CachedHitMaterializeRequest`; write payloads[1] to it when present
- [ ] Wire it through three call sites in `hit_branches.rs`
- [ ] On miss path in `pipeline.rs`: after `parse_depfile_path` for `UserSpecified` / `UserDefault`, capture the depfile bytes and pass them to `store_single_output`
- [ ] Extend `store_single_output` (in `miss_store.rs`) with optional second output for depfile bytes
- [ ] Thread `dep_flags` into the hit-probe constructors so each site can derive `current_depfile_dest`
- [ ] `soldr cargo fmt --all`
- [ ] `soldr cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `./test` (unit)
- [ ] Commit test (RED commit), then impl (GREEN commit) — conventional commits
- [ ] Push branch + open PR

## Notes

- The bug: zccache restores `.obj` on hit but **never writes the `.d` depfile** to the user's `-MF` path. Downstream `deps = gcc` build systems then record zero deps for the object and stop recompiling when headers change.
- Fix only applies to `UserSpecified` and `UserDefault` strategies. `Injected` does not need it (the user never asked for the file). MSVC `ShowIncludes` / `Unsupported` are out of scope.
- Cache mtime preservation: depfile write must follow the same `write_payloads_par_with_mtime_floor` path the `.obj` already uses. No `now()` stamps.
- Legacy artifacts (1 output) must keep working — only act when 2 outputs AND `current_depfile_dest` is Some.

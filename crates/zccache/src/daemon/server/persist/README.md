# `persist/`

Artifact pack format, atomic writes, hardlinking, cached output materialization.

Originally a single 1K-LOC `persist.rs`; split per domain so each file stays
well under the 1,000 LOC cap. Public surface is preserved — the parent
`use persist::*;` glob still sees every `pub(super)` symbol because `mod.rs`
re-exports `*` from each submodule.

## Layout

- **[`mod.rs`](mod.rs)** — Module wiring + re-exports.
- **[`artifact_io.rs`](artifact_io.rs)** — Atomic writes
  (`persist_artifact_output`, `persist_artifact_file`,
  `persist_artifact_paths`, `replace_artifact_cache_file`),
  `PersistArtifactFileStats`, error enrichment (`enrich_persist_err`,
  issue #728), and the Windows AV-scanner retry helper (issue #490).
- **[`pack.rs`](pack.rs)** — Experimental `.pack` artifact format gated by
  `ZCCACHE_PACK_ARTIFACTS`: magic header, builder, parser, per-payload
  extractor.
- **[`write_cached.rs`](write_cached.rs)** — Materialize cached output to its
  target path (`write_cached_output`, `write_cached_file`,
  `write_cached_payload`, `write_payloads_par`,
  `write_payloads_par_with_mtime_floor`).
- **[`hardlink.rs`](hardlink.rs)** — Cross-platform hardlink helpers
  (`break_output_hardlink_before_compile`, `hard_link_count`,
  `same_file`, Windows `get_file_id`).
- **[`mtime.rs`](mtime.rs)** — Mtime preservation + sibling-floor refinement
  (`touch_mtime`, `floor_materialized_outputs_to_input_max`,
  `floor_artifact_mtime_to_sibling_max`). See the iter7 invariant in
  `CLAUDE.md` and issues #466 / #467.
- **[`tests.rs`](tests.rs)** — Unit tests for the mtime-floor + batch
  materializer paths (issues #466 / #467 / #599).

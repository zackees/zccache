# zccache-depgraph

Internal unpublished crate for dependency graph tracking and include-aware cache invalidation.

The public module surface is exported from `src/lib.rs` so the facade crate can re-export it as `zccache::depgraph`.

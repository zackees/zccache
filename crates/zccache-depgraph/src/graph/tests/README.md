# Dependency graph tests

Focused regression tests for dependency-graph behavior that are too specialized
for the main graph test module.

`hash_precedence.rs` proves that measured source/header/force-include/extern
changes override watcher silence in ordinary, diagnostic and metadata-alias verdicts.

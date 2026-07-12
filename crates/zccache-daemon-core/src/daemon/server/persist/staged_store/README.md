# Staged store internals

- `maintenance.rs` scans and evicts complete immutable generations.
- `materialize.rs` restores requested outputs with physical-tier observations.
- `fault.rs` provides path-scoped deterministic fault injection in tests only.

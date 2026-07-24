# Staged store internals

- `maintenance.rs` scans, evicts, and safely clears complete immutable
  generations.
- `materialize.rs` restores requested outputs with physical-tier observations.
- `root_safety.rs` validates the exact staged root and removes links/reparse
  points without traversal.
- `fault.rs` provides path-scoped deterministic fault injection in tests only.
- `hook.rs` provides deterministic test synchronization around publication.

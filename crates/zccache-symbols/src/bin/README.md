# zccache-symbols binaries

- `stamp.rs` — `zccache-stamp` helper invoked by CI to append the 96-byte
  release footer (git SHA, version, target triple, build timestamp,
  magic) to a built binary. Must run after stripping and before packaging.

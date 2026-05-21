# zccache-symbols tests

Integration tests for the marker + sidecar pieces. The fetch side is
covered by `zccache-cli`'s own tests.

- `stamp_roundtrip.rs` — invoke `zccache-stamp` on a fixture file, then
  parse the marker back via `read_marker_from_path`.

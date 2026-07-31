# ban_discarded_write_result

This Dylint rejects a `Result` from a write-ish call being thrown away in the
daemon's persistence paths — either via `let _ = <write call>;` or via a
statement-position `<write call>.ok();`.

The bug class is #1163: `miss_store.rs` sent artifact-index records with
`let _ = …index_writer_tx.send(…)` and then unconditionally recorded the
artifact as cached. If the send failed, the artifact never reached
`index.bin` and the next daemon start re-missed it — silently. The same
shape applies to a dropped `write`/`rename`/`persist`/`flush`: the caller
proceeds as if durable state was written when it was not.

## Scope

Only the daemon's state-mutation modules are checked (see
`DAEMON_SOURCE_PREFIXES` in `src/lib.rs`):

- `crates/zccache-daemon-core/src/daemon/server/persist/`
- `crates/zccache-daemon-core/src/daemon/server/handle_compile/miss_store.rs`
- `crates/zccache-daemon-core/src/daemon/server/wal.rs`

Files under a `tests/` directory or named `*_tests.rs` are out of scope.

## Callee names

The matched set is deliberately tight so the lint does not drown in
false positives from best-effort cleanup:

`write`, `write_all`, `send`, `rename`, `persist`, `flush`, `sync_all`,
`set_len`.

Cleanup-shaped calls (`remove_file`, `remove_dir_all`, `set_readonly`) are
intentionally **not** matched — discarding those is the normal idiom on an
already-failing path.

## Fixing a hit

Propagate the error with `?`, or handle it explicitly and gate the
follow-on state mutation on success:

```rust
if let Err(error) = tx.send(record) {
    tracing::warn!(%error, "artifact index record dropped");
    return Err(error.into());
}
state.artifacts.insert(key, entry);
```

If a site is genuinely fire-and-forget, add its path tail to
`src/allowlist.txt` **with a comment explaining why the dropped error is
safe**.

# compile_journal

JSONL compile journal for build replay. Records every compile/link command
with enough detail to replay the entire build. One JSON object per line,
written to `{cache_dir}/logs/compile_journal.jsonl`.

Originally a single 2,072-LOC `compile_journal.rs`; split here so each file
stays well under 1,000 LOC. No behavior change — purely a code split. Public
API (`pub use` re-exports from `mod.rs`) is unchanged.

## Files

- **mod.rs** — Public types (`JournalEntry`, `MissDiff`, `SelfProfileNs`,
  `SelfProfileSpans`, `JournalContext`, `CompileJournal`) and the inline
  `pub mod miss_reason` constant module. Re-exports `derive_*` helpers and
  `extract_outcome` so the public path
  `zccache_daemon::compile_journal::<item>` stays identical.
- **derive.rs** — Pure helpers that parse rustc-style argv into the canonical
  schema strings (`derive_crate_name`, `derive_crate_type`,
  `derive_output_ext`).
- **outcome.rs** — `extract_outcome`: maps a `Response` to
  `(outcome_str, exit_code, default_miss_reason)`. The canonical translation
  point for issue #322 (every miss carries a reason).
- **journal_thread.rs** — Background writer thread (`journal_thread`),
  `JournalMessage` enum, rotation (`rotate_journal`,
  `JOURNAL_MAX_SIZE`/`JOURNAL_MAX_FILES`), and GC (`gc_journal_files`).
- **tests/** — All `#[cfg(test)]` tests, grouped per subject.

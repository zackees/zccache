# compile_journal/tests

Unit tests for `compile_journal`, grouped per subject so each file stays
well under 1,000 LOC. Originally part of a single 2,072-LOC
`compile_journal.rs`.

## Files

- **mod.rs** — declares the per-subject submodules and owns the small
  `wait_for_lines` polling helper and `legacy_entry` constructor shared
  across the file-write / serialization tests.
- **derive.rs** — `derive_crate_name`, `derive_crate_type`,
  `derive_output_ext` argv parsers.
- **outcome.rs** — `extract_outcome` `Response`-to-tuple mapping (basic
  variants + edge cases, including the issue-#322 `miss_reason` defaults).
- **miss_reason.rs** — `miss_reason::*` constants, `MissDiff`
  serialization, and the end-to-end JSONL miss-reason wiring tests.
- **entry.rs** — `JournalEntry` serialization (legacy + extended profile
  fields), `SelfProfileSpans`, `with_profile_fields`.
- **journal_file.rs** — `CompileJournal` end-to-end: file writes,
  per-session journals, close/reopen, concurrent logging, rotation, GC.

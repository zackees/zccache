# depfile

Parser for GNU make dependency files (`.d` files) emitted by GCC/Clang via `-MD -MF`.

## Submodules

- **`mod.rs`** — public API and re-exports (`parse_depfile`, `parse_depfile_path`, `DepfileError`, `DepfileStrategy`, `prepare_depfile`, `user_depfile_destination`, `canonicalize_path`, `strip_win_prefix`).
- **`error.rs`** — `DepfileError` enum (`Io` / `Malformed`) and `Display`/`Error`/`From<io::Error>` impls.
- **`parse.rs`** — `parse_depfile` / `parse_depfile_path` entry points plus the internal `join_continuations`, `find_separator_colon`, `split_and_unescape` helpers.
- **`canonicalize.rs`** — process-wide canonicalize cache (issue #573) and the shared `canonicalize_path` / `strip_win_prefix` helpers also used by `show_includes.rs`.
- **`strategy.rs`** — `DepfileStrategy`, `user_depfile_destination`, and `prepare_depfile` (decides whether to inject `-MD -MF`, defer to user-provided flags, etc.).
- **`tests.rs`** — unit + behavioural tests for the parser, strategy, and canonicalize cache.

The split was made when `depfile.rs` crossed the 1,000-LOC guard. The public path `crate::depgraph::depfile::<Name>` is unchanged thanks to the `pub use` re-exports in `mod.rs`.

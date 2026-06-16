# cli/commands/args

clap-derive argument-parsing types for the `zccache` binary.

Split out of the former single-file `args.rs` (PR splitting #767) to stay
under the 1,000-LOC `loc_guard.py` cap. The public path
`cli::commands::args::<Name>` is preserved via re-exports in `mod.rs`, so
callers outside this directory are unchanged.

## Layout

- `mod.rs` — top-level `Cli` parser, the `Commands` enum (every
  subcommand the binary accepts), the `KNOWN_SUBCOMMANDS` table used by
  the wrap-mode auto-detect in `cli::commands::run`, and re-exports of
  the per-subcommand enums declared in `subcommands.rs`.
- `subcommands.rs` — every nested `clap::Subcommand` enum referenced
  from `Commands` (`CacheCommands`, `MesonCommands`, `SymbolsCommands`,
  `DefenderExclusionsCommands`, `CargoRegistryCommands`, `KvCommands`,
  `FpCommands`, `GhaCacheCommands`, `RustPlanCommands`) plus the
  `RustPlanBackendArg` value-enum.

## Invariant

`KNOWN_SUBCOMMANDS` in `mod.rs` MUST stay in sync with the `Commands`
enum — any name added to the enum without a matching entry routes into
wrap mode and surfaces as "daemon error: failed to run compiler: program
not found". The `known_subcommands_matches_clap_enum` test in
`cli::commands::tests::args_parsing` enforces the contract.

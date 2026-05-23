# cli

Subcommand implementations and dispatch glue for the `zccache` binary.
`mod.rs` owns the clap `Cli`/`Commands` definitions and the dispatch match;
each sibling module implements one logical command group (analyze, session,
warm, rust-plan, fp, wrap, etc.) so no single file is too large to navigate.

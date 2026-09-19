# enforce_platform_boundary source

`lib.rs` contains the pre-expansion source-boundary lint. It classifies each
repo-relative source path and rejects host cfg, native imports, and concrete
platform references before inactive branches are stripped.

| Path | Scope | Host mechanics |
|---|---|---|
| approved `crates/**/src/platform.rs` adapters | product policy | allowed |
| kernal-api dependency | native implementation | not inspected here |
| every other production `crates/**` source | product | denied with no baseline or exceptions |
| tests, benches, vendor, fixtures, test-support | non-production | not inspected |

The lint matches pre-expansion path names, so its UI fixtures can exercise
native-looking paths without depending on host-native crates.

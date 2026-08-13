# enforce_platform_boundary — source

- `lib.rs` — the early (pre-expansion) lint: path classification, the
  forbidden-construct matcher, the exact-occurrence baseline ratchet, and the
  baseline-staleness check. Runs before cfg-stripping and macro expansion so
  inactive host branches (e.g. Windows code on Linux CI) are still inspected.
- `baseline.txt` — the transitional exact-occurrence baseline. See the lint
  README for the format and ratchet rules.

Classification of a compiled file by its repo-relative path:

| Path | Scope | Host cfg / native imports / concrete refs |
|---|---|---|
| `crates/zccache-platform/src/lib.rs` | selector | allowed (the single cfg_select! site) |
| `crates/zccache-platform/src/platform_win(.rs)/**` etc. | concrete tree | allowed |
| `crates/zccache-platform/src/platform.rs`, `src/platform/**` | neutral facade | denied; `platform_imp` (never concrete names) is the only bridge |
| every other `crates/**` production file | product | denied; existing exact occurrences matched by `baseline.txt`, anything else is an error |
| `dylints/enforce_platform_boundary/ui/**` | lint fixtures | denied (no baseline) |
| tests/, benches/, vendor/, perf fixtures, zccache-test-support | out of scope | not inspected |

The lint matches **pre-expansion** path *names*: it cannot distinguish a
local `mod libc` shadow from the real crate, which is exactly what the UI
fixtures exploit to stay host-independent.

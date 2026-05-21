# Compile Journal Schema

The daemon writes one JSON object per line to
`<cache_dir>/logs/compile_journal.jsonl` and, when a session opted in,
`<cache_dir>/logs/last-session.jsonl`. This document is the authoritative
reference for the fields consumers (setup-soldr, dashboards, post-mortem
scripts) can rely on.

## Record shape

| Field          | Type            | Always present | Notes |
|----------------|-----------------|----------------|-------|
| `ts`           | string          | yes            | ISO 8601 UTC timestamp of the record write. |
| `outcome`      | string          | yes            | One of `hit`, `miss`, `error`, `link_hit`, `link_miss`. |
| `compiler`     | string          | yes            | Absolute path to the compiler binary as the client invoked it. |
| `args`         | array of string | yes            | Full argument list, suitable for replay. |
| `cwd`          | string          | yes            | Working directory at request time. |
| `env`          | array of `[k, v]` | no           | Omitted when the client passed `None`. |
| `exit_code`    | integer         | yes            | Process exit code. `-1` is reserved for daemon-side errors. |
| `session_id`   | string \| null  | yes            | UUID for session-attached requests; `null` for ephemeral. |
| `latency_ns`   | integer         | yes            | Wall-clock nanoseconds (per the project's `_ns` convention). |
| `crate_name`   | string          | no             | Populated when parseable from `--crate-name` (rustc). |
| `crate_type`   | string          | no             | Canonical: `lib`, `bin`, `proc-macro`, `build-script`, `test`, `bench`, `example`. |
| `output_ext`   | string          | no             | Derived from `crate_type` — `rlib`, `exe`, `so`, etc. |
| `miss_reason`  | string          | on misses only | See below. |
| `miss_diff`    | object          | no             | Evidence bucket; only the dimension that changed is populated. |
| `self_profile_ns` | object       | no             | Sub-phase timings (`hash_inputs`, `lookup`, `decompress`, `store`). |

## `miss_reason` enum

Per issue #322 every `outcome: miss` and `outcome: link_miss` record
carries a `miss_reason` so consumers can build histograms over a finite
set instead of guessing. The closed set today:

| Value                          | Meaning |
|--------------------------------|---------|
| `context_not_found`            | The daemon has no dep-graph context for this compile unit (cold cache; first time this daemon process has seen the crate). Common when the daemon was restarted between builds and persistence didn't run. |
| `input_fingerprint_mismatch`   | A context exists but the source/header/flag fingerprint differs from the cached entry. The most actionable bucket — points at unexpected input drift. |
| `no_artifact_for_key`          | The cache key resolved, but the artifact bytes on disk are gone (GC'd, never persisted, or corrupted). |
| `version_skew`                 | Compiler version, target triple, or zccache schema differs from the cached entry. |
| `uncacheable_input`            | The invocation parsed but is intrinsically uncacheable (PGO profile emit, host-specific link flags, etc.). |
| `unknown`                      | Fallback. Emitted whenever the daemon detected a miss but has not yet attributed a precise reason. Follow-up work narrows `unknown` into the concrete buckets above. Consumers should still treat the field as present so dashboards don't crash on absent keys. |

Hit and error records never carry `miss_reason`.

The Rust source of truth is the `miss_reason` module in
`crates/zccache-daemon/src/compile_journal.rs`. `miss_reason::ALL` is the
append-only iteration of the closed set.

## Stability & versioning

The journal is **additive-by-default**: new optional fields may appear in
later releases without bumping any version. Consumers should ignore
unknown keys. Removal or rename of a documented field is a breaking
change and would require a coordinated schema-version field.

`miss_reason` itself is allowed to gain new variants over time. Consumers
that switch on the value should always have a default arm — treat
unrecognized variants the same as `unknown`.

## Related

- Issue [#322](https://github.com/zackees/zccache/issues/322) — the consumer-side need that motivated this schema.
- `docs/architecture/runtime.md` — where the journal sits in the daemon lifecycle.
- `docs/DESIGN_DECISIONS.md` — DD-018 on protocol version bumps and IPC roundtrips (no-roundtrip rule applies to live IPC, not to journal records).

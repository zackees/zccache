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
| `outcome`      | string          | yes            | One of `hit`, `miss`, `error`, `cached_error`, `link_hit`, `link_miss`. |
| `compiler`     | string          | yes            | Absolute path to the compiler binary as the client invoked it. |
| `args`         | array of string | yes            | Full argument list, suitable for diagnostic replay when combined with an independently captured environment. |
| `cwd`          | string          | yes            | Working directory at request time. |
| `env`          | array of `[k, v]` | no           | Sanitized build-diagnostic allowlist. Secret-looking names/values and all non-allowlisted variables are omitted. |
| `exit_code`    | integer         | yes            | Process exit code. `-1` is reserved for daemon-side errors. |
| `session_id`   | string \| null  | yes            | UUID for session-attached requests; `null` for ephemeral. |
| `latency_ns`   | integer         | yes            | Wall-clock nanoseconds (per the project's `_ns` convention). |
| `context_key`  | string          | no             | Full root-normalized dep-graph context hash. Present once cache-key construction completes; useful for comparing ephemeral cross-worktree requests. |
| `crate_name`   | string          | no             | Populated when parseable from `--crate-name` (rustc). |
| `crate_type`   | string          | no             | Canonical: `lib`, `bin`, `proc-macro`, `build-script`, `test`, `bench`, `example`. |
| `output_ext`   | string          | no             | Derived from `crate_type` — `rlib`, `exe`, `so`, etc. |
| `miss_reason`  | string          | on misses only | See below. |
| `miss_diff`    | object          | no             | Evidence bucket; only the dimension that changed is populated. |
| `self_profile_ns` | object       | no             | Sub-phase timings (`hash_inputs`, `lookup`, `decompress`, `store`). |

## Environment security and replay

The journal is a durable diagnostic record, not a credential store. zccache
passes the original request environment to the compiler unchanged, but writes
only a narrow diagnostic subset to `env`: compiler/linker selections and
flags, Cargo package/configuration metadata, target/profile fields, and a few
platform SDK paths. The exact allowlist is defined in
`daemon/compile_journal/env.rs`.

Filtering is deny-first. Names that indicate tokens, passwords, secrets, keys,
authentication, credentials, or cookies are omitted even when they have an
otherwise allowed prefix such as `CARGO_FEATURE_` or `DEP_`. Values resembling
GitHub/PyPI/npm/cloud tokens, bearer/basic credentials, JWTs, private keys,
credential-bearing URLs, or opaque high-entropy tokens are also omitted.
Omission is preferred to a redaction marker because credential variable names
can themselves reveal sensitive service usage.

This is a compatibility change for replay consumers: the persisted `env` is
not sufficient to reconstruct the complete compiler process environment.
Consumers may continue to use `compiler`, `args`, and `cwd`, but exact replay
must supply an independently captured trusted environment. Existing records
with a full `env` remain parseable; consumers should already tolerate a missing
optional `env` field. The versioned cross-repo regression fixture is
`crates/zccache-daemon-core/src/daemon/compile_journal/tests/compile_journal_env_security_v1.json`.

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
| `uncacheable_input`            | The invocation parsed but is intrinsically uncacheable or was rejected before cache-key construction (version probes, stdin-only probes, unsupported flags, etc.). |
| `destination_write_failed`     | The cached artifact remains valid, but materializing it at the caller's requested output path failed. This is a soft miss and never authorizes cache eviction. |
| `unknown`                      | Fallback. Emitted whenever the daemon detected a miss but has not yet attributed a precise reason. Follow-up work narrows `unknown` into the concrete buckets above. Consumers should still treat the field as present so dashboards don't crash on absent keys. |

Hit and error records never carry `miss_reason`. `cached_error` records are
replayed rustc failures; they are distinct from fresh `error` records and
also omit `miss_reason`.

The Rust source of truth is the `miss_reason` module in
`crates/zccache-daemon-core/src/daemon/compile_journal/mod.rs`.
`miss_reason::ALL` is the
append-only iteration of the closed set.

## Issue #256: `session-start --profile` and the extended schema

The optional fields `crate_name`, `crate_type`, `output_ext`,
`miss_diff`, and `self_profile_ns` are populated only when the
session was created with `zccache session-start --profile`.
Without the flag, the journal record uses the legacy lean shape
and incurs zero new allocations on the daemon hot path.

`crate_name`, `crate_type`, and `output_ext` are derived from the
rustc argument vector by the `derive_crate_name` /
`derive_crate_type` / `derive_output_ext` helpers in
`crates/zccache-daemon-core/src/daemon/compile_journal/derive.rs`.
`crate_type` takes
one of `lib`, `bin`, `proc-macro`, `build-script`, `test`,
`bench`, `example`; the matching `output_ext` is `rlib`, `exe`,
`so` for proc-macro, etc.

`self_profile_ns` is a four-bucket nanosecond histogram of the
daemon-internal phases for a single compile:

- `hash_inputs` -- time spent computing the cache key inputs.
- `lookup` -- time spent in the artifact/depgraph lookup path.
- `decompress` -- time spent decompressing an artifact on a hit.
- `store` -- time spent persisting an artifact on a miss.

Buckets that did not run for a given compile serialize as `0`.

`miss_diff` is an evidence bucket: only the dimension that flipped
is populated. Empty arrays are omitted from serialization.

The `zccache analyze` subcommand rolls journals up offline and
reads only the documented fields; it tolerates malformed lines
(emits a stderr warning and skips the row) and missing journals
(exits zero with a `(no journal)` message).

## Engine phase profiling

`session-stats --json` and `session-end --json` include `phase_profile` when
the daemon can report cache-engine phase totals. Legacy hit/miss fields remain
daemon-wide; the nested staged-pipeline summary contains only work attributed
to that tracked session. Use
`zccache engine-profile <stats-json>` to render the hit/miss phase breakdown
from `last-session-stats.json` or captured session-stats JSON. Add `--json`
for the stable machine-readable form.

This is separate from Tokio Console. `engine-profile` attributes cache-engine
work such as hashing, depgraph checks, artifact lookup, output writes, compiler
execution, include scanning, and artifact storage. Tokio Console is for live
Tokio runtime symptoms such as blocked tasks, long polls, wakeups, timers, and
resource contention.

## Retention and file ownership (issue #1165)

Two different files, two different owners. This is the part that decides who is
responsible for deleting anything.

**Daemon-owned, bounded by the daemon:**

| File | Bound |
|---|---|
| `<cache_dir>/logs/compile_journal.jsonl` | rotated by the journal writer's own size/GC policy |
| `<cache_dir>/logs/daemon-lifecycle.log` | 1 MiB live + one `.1` archive |
| `<cache_dir>/logs/audit.jsonl` | 50 MB × 3 files |
| `<cache_dir>/crashes/` | newest `CRASH_DUMP_KEEP` dumps, `.reported` markers reaped with their dump |
| `$ZCCACHE_LOG_FILE` (opt-in tracing sink) | `ZCCACHE_LOG_FILE_MAX_BYTES` live + one archive |

**Caller-owned, and deliberately never touched by the daemon:** the per-session
journal at `SessionConfig.journal_path`.

That path is **supplied by the client** on `SessionStart`, and the daemon never
defaults it — omit it and no per-session journal is written at all. `close_session`
therefore drops the handle and leaves the file exactly where the caller asked for
it.

This is by design, and the alternative was considered and rejected. A retention
policy here would mean the daemon enumerating a directory *the client chose*
(`/tmp`, a home directory, a build tree) and unlinking files by a filter it
invented — session journal names are client-chosen with no shared prefix, so any
filter broad enough to catch real journals is broad enough to catch unrelated
user files. Bounding one small JSONL per session is not worth a mechanism that
can delete outside the cache root.

**If you pass `journal_path`, you own retention.** A caller that mints a fresh
path per session (a CI job, a build wrapper) must rotate or clean them itself.
A caller that reuses one path, or omits it, has nothing to clean.

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
- `crates/zccache/src/audit.rs` — durable embedded audit schema for causal events, findings, and run manifests. The audit event stream may reference this compile journal as evidence via event IDs or artifact paths, but the compile journal remains the cache-specific replay record.
- `docs/architecture/runtime.md` — where the journal sits in the daemon lifecycle.
- `docs/DESIGN_DECISIONS.md` — DD-018 on protocol version bumps and IPC roundtrips (no-roundtrip rule applies to live IPC, not to journal records).

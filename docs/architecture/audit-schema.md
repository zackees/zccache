# Durable audit schema for embedded zccache integrations

> **Issue:** zccache#906 — the schema half of the embedded audit work
> tracked under the umbrella at zccache#929. Companion to
> [`embedded-service.md`](embedded-service.md) (the contract) and to the
> follow-up writer issue zccache#926 (the hot-path emission). The hot-path
> writer is intentionally **not** in this doc — it lands separately.

This document is the public contract for the JSON Lines (JSONL) audit
events that an embedded zccache emits when a host product configures
`AuditConfig::mode > Off`. The on-disk source of truth is
[`crates/zccache/src/audit.rs`](../../crates/zccache/src/audit.rs); this
doc explains the wire shape, the compatibility policy, the redaction
contract, and how host products correlate event causality across crate
boundaries.

## Schema identity

| Constant | Value | Source |
|---|---|---|
| Schema identifier | `soldr.audit.v1` | `audit::AUDIT_SCHEMA` |
| Schema version | `1` (u32) | `audit::AUDIT_SCHEMA_VERSION` |

Every emitted event carries the identifier and version as top-level
fields. Consumers MUST refuse events whose `schema` does not equal the
identifier they were built against; they MAY accept events with a
`schema_version` they don't recognize as long as the rest of the
top-level shape parses (see "Additive compatibility" below).

## Event shape

```jsonc
{
  "schema": "soldr.audit.v1",
  "schema_version": 1,
  "timestamp_ms": 1719423712345,
  "level": "info",
  "category": "zccache.compile",
  "event": "compile.finished",
  "context": {
    "run_id": "01HQK4ZJVXJ2W6BX5R5VVZGN6Y",
    "build_id": "01HQK4ZJVX0M",
    "trace_id": "0123456789abcdef0123456789abcdef",
    "span_id": "0123456789abcdef",
    "parent_span_id": null,
    "command_id": "cargo-build-soldr-cli",
    "compile_id": "01HQK4ZK0F2WC3R4PMSXVTKE7B",
    "session_id": "01HQK4ZJVX1ZSN8KW8M40H7Y8M"
  },
  "fields": {
    "crate_name": "serde",
    "target_triple": "x86_64-pc-windows-msvc",
    "exit_code": 0,
    "duration_ms": 187,
    "cache_outcome": "miss",
    "bytes_written": 4194304
  }
}
```

### Top-level fields

| Field | Type | Required? | Meaning |
|---|---|---|---|
| `schema` | `string` | yes | Always `"soldr.audit.v1"` for this contract version. |
| `schema_version` | `u32` | yes | Always `1`. Bumped on a breaking change (see "Additive compatibility"). |
| `timestamp_ms` | `u64` | yes | Unix epoch milliseconds at event-creation time. |
| `level` | `string` enum | yes | One of `debug`, `info`, `warn`, `error`. Maps 1-1 to `AuditLevel`. |
| `category` | `string` | yes | Dotted-path category (see "Event categories"). Encoded via `AuditCategory::new` so it can't be empty. |
| `event` | `string` | yes | Event name within the category. Encoded via `AuditEventName::new` so it can't be empty. |
| `context` | object | yes | The causal identifiers — see "AuditContext" below. |
| `fields` | object | yes | Free-form structured payload keyed by string. Empty `{}` is valid. |
| `findings` | array | optional | Structured `AuditFinding` records the producer wants the consumer to surface (recommendations, regressions, evidence). |

### `AuditContext`

The causal identifiers that let a consumer reconstruct host → zccache
order across an entire build. Mirrors `audit::AuditContext` exactly.

| Field | Type | Required? | Meaning |
|---|---|---|---|
| `run_id` | `string` | yes | Stable identifier for the *whole* host run — typically a UUID or ULID generated once when the host starts a top-level build. |
| `build_id` | `string` | optional | Sub-build inside a multi-build run (e.g. `cargo build --workspace` builds multiple crates). |
| `trace_id` | `string` | yes | Distributed-tracing-style trace identifier. Hosts that already emit OTLP / W3C trace context SHOULD reuse the same ID here so audit events join their trace pane. |
| `span_id` | `string` | optional | This event's span. Hosts that don't emit trace spans MAY leave this null. |
| `parent_span_id` | `string` | optional | The span that caused this one. |
| `command_id` | `string` | optional | Stable identifier for the host-level command (e.g. `cargo-build-soldr-cli`). Stable across re-runs — used for fixture-equivalence. |
| `compile_id` | `string` | optional | zccache's per-compile identifier — present on events emitted by the compile pipeline. |
| `session_id` | `string` | optional | The build-session identifier — soldr's `BuildSessionStart.session_id`, fbuild's equivalent. |

Hosts MUST supply `run_id` + `trace_id`; everything else is best-effort.
The two-required-fields contract is the bare minimum for "reconstruct
soldr/fbuild → zccache causality" (#906 acceptance criterion).

The Rust type uses `AuditId(pub String)` rather than `Uuid` so hosts
can use whatever identifier system they already have. Anything
non-empty is accepted; the type prevents only the empty-string mistake.

### Event categories

`category` uses a dotted path so consumers can subscribe to a subtree
(`zccache.*`, `zccache.compile.*`, etc.). The reserved prefixes are:

| Prefix | Owner | Examples |
|---|---|---|
| `host.lifecycle` | host (soldr, fbuild) | `host.lifecycle.run.started`, `host.lifecycle.run.finished` |
| `host.plan` | host | `host.plan.resolved`, `host.plan.invalidated` |
| `host.execute` | host | `host.execute.cargo.started`, `host.execute.cargo.finished` |
| `host.scheduler` | host | `host.scheduler.dispatch`, `host.scheduler.priority` |
| `host.process` | host | `host.process.spawn`, `host.process.exit` |
| `zccache.compile` | zccache | `compile.started`, `compile.finished`, `compile.cancelled` |
| `zccache.cache` | zccache | `cache.lookup`, `cache.hit`, `cache.miss`, `cache.store` |
| `zccache.depgraph` | zccache | `depgraph.check`, `depgraph.update` |
| `zccache.artifact` | zccache | `artifact.write`, `artifact.evict` |
| `zccache.compiler_exec` | zccache | `compiler.spawn`, `compiler.exit` |
| `zccache.runtime` | zccache | `runtime.task.spawned`, `runtime.task.cancelled` |
| `system` | shared | `system.io.disk_full`, `system.cpu.saturated` |

A host product is free to add its own categories; the recommendation
is to use a host-specific prefix (`soldr.*`, `fbuild.*`) so they don't
collide with future zccache reservations.

## Audit modes

`AuditConfig::mode` selects how much detail flows to the writer. The
enum lives at `audit::AuditMode` and serializes as snake_case:

| Mode | When to use | Effect on writer |
|---|---|---|
| `off` | Production builds that don't need durable evidence. | Writer task never spawns; calls to emit are a no-op. |
| `summary` | CI runs that want a one-line per-build summary. | Events accumulate in an in-memory summary structure; only the final summary lands at `flush()`. |
| `normal` | The interactive default. | All `info+` events are written. |
| `verbose` | Local debugging / perf-cluster runs. | Adds `debug` events. |
| `forensic` | Capturing fixtures for the audit test suite. | All events including internal-only diagnostics. Higher disk cost. |

The writer respects the mode at start time. Changing the mode mid-run
requires `flush()` + `shutdown()` + `start()` with the new config.

## Sink failure policy

`AuditConfig::sink_policy` lives at `audit::AuditSinkPolicy`. Each
policy answers the question "what does the embedded service do when
the writer cannot drain fast enough"?

| Policy | Behavior on backpressure |
|---|---|
| `block` | Suspend the event-emitting code until the writer drains. Strongest correctness guarantee; risks stalling the compile pipeline. |
| `drop_low_priority` | Drop `debug` and `info` events; preserve `warn` and `error`. Bumps `audit_lost_events` counter. |
| `degrade` | Switch to in-memory summary mode for the rest of the run; final summary at flush. Best-effort. |
| `fail_lossless` | Treat backpressure as fatal: surface the error to the host. Default. |

`fail_lossless` is the documented default because the embedded contract
explicitly trades latency for fidelity — a build that silently loses
audit data has lost the property the host adopted the contract for.

## Additive compatibility

The `schema_version: 1` constant binds three rules to every future
change:

1. **Adding optional fields is allowed within v1.** A consumer compiled
   against any v1 build MUST ignore unknown top-level fields and
   unknown `fields.*` keys. The Rust `serde` derive sets
   `#[serde(deny_unknown_fields)]` to `false` (the default) on every
   schema struct, so this is enforced at the parsing layer.
2. **Removing or renaming a field requires a schema-version bump.** A
   field that has shipped in a release tarball is a contract; the next
   change that touches it bumps `AUDIT_SCHEMA_VERSION` to `2` and
   updates the identifier to `soldr.audit.v2`. Consumers MAY read both
   versions side-by-side during a transition window.
3. **Renaming a category or event is a breaking change.** Same rule as
   field renames — bump the version. New categories and events under
   reserved prefixes are additive within v1.

The intent is that a host product can lock to v1 for the lifetime of
its current major release and not worry about silent shape changes.

## Redaction

`AuditConfig::redaction` lives at `audit::AuditRedactionPolicy`.

| Field | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Master switch. Disable only for fixture-capture under controlled conditions. |
| `redact_env_keys` | `["PATH", "HOME", "USERPROFILE", …]` | Env var names whose values are replaced with `replacement`. |
| `redact_field_keys` | `["token", "secret", "password", "api_key", …]` | Field keys whose values are replaced regardless of position in `fields`. |
| `allow_field_keys` | `[]` | An explicit allowlist that overrides `redact_field_keys`. Used to surface specific environment values for diagnostics without weakening the default. |
| `replacement` | `"<redacted>"` | The string substituted for redacted values. |

The redaction step runs **at event-construction time**, before the
event reaches the writer queue. A redacted event on disk never carried
the sensitive value in memory beyond the construction site.

Callers use `AuditEvent::apply_redaction(&policy)` to apply the policy
to an event-in-progress. The writer applies it automatically when the
event reaches the queue, but explicit calls are recommended for any
event constructed outside the standard hot path.

## Fixtures

See [`crates/zccache/tests/audit-fixtures/`](../../crates/zccache/tests/audit-fixtures/)
for the canonical JSONL examples. The directory ships three baseline
fixtures aligned with the three most common host-validation scenarios
(per the [vendored-hotfix-workflow](vendored-hotfix-workflow.md)):

| Fixture | Captures |
|---|---|
| `embedded-cold-compile.jsonl` | Single rustc invocation, cold cache (miss → store). |
| `embedded-warm-compile.jsonl` | Same invocation re-run against a warm cache (hit). |
| `embedded-cancelled-compile.jsonl` | A compile aborted via `AuditContext` cancellation (zccache#923) — the terminal event has `mode: "cancelled"` in `fields`. |

The fixtures are normalized: timestamps are zeroed, identifiers are
deterministic stand-ins, and host-specific paths are replaced with
generic tokens. The corresponding test asserts shape-equivalence:
running the same scenario against the embedded service produces a
JSONL that round-trips through the same normalizer and equals the
fixture byte-for-byte.

Hosts that surface a new validation scenario during the
[vendored-hotfix-workflow](vendored-hotfix-workflow.md) contribute the
captured trace back as a new fixture under the same naming convention.

## What this schema is NOT

- It is **not** a substitute for `tracing` spans. Tokio Console + the
  in-process `tracing` subscriber are still the primary live debugging
  tools. The audit log is the durable source of truth that survives
  the process exit.
- It is **not** a profiling pipeline. Events are sampled at semantic
  milestones (compile start / finish, cache lookup, artifact write).
  Per-syscall granularity is the perf-cluster's job.
- It is **not** a security audit log in the compliance sense. Hosts
  that need SOX / SOC2-style logging build that on top of the audit
  events; the embedded service does not claim WORM storage, signing,
  or chain-of-custody guarantees.

## Cross-references

- [`embedded-service.md`](embedded-service.md) — the embedded-service
  contract this schema operates inside.
- [`vendored-hotfix-workflow.md`](vendored-hotfix-workflow.md) — the
  loop that surfaces audit fixtures from host validation runs.
- zccache#926 — the writer that consumes this schema on the hot path.
- zccache#910 — the operator-facing API (`soldr audit run`, etc.) that
  reads these JSONL files.
- zccache#929 — the embedded-service umbrella meta.

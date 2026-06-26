# Audit operator API contract

> **Issue:** zccache#910 — the soldr-facing contract for agent-driven
> performance analysis using the durable audit data emitted by the
> embedded service. Companion to [`audit-schema.md`](audit-schema.md)
> (the wire format) and zccache#926 (the writer). The producer ships in
> zccache; the operator commands ship in soldr.

This document is the public contract for the three commands that an
agent uses to drive an audited run end-to-end:

```
soldr audit capabilities --json
soldr audit run [options] -- <subcommand>
soldr audit analyze <audit-run-dir> --json
```

The commands live in the soldr CLI (the soldr-side implementation
tracks against soldr#977 Phase 4). This doc is the cross-repo
agreement so the implementation has a stable target. The contract is
versioned by the same `soldr.audit.v1` schema identifier as the JSONL
emission — bumping the schema bumps the operator API in lockstep.

## Why this is a contract, not "implement whatever feels right"

Agents that drive `soldr audit run` then parse the output cannot
afford for the output shape to drift between soldr releases. The same
applies to AI / operator tooling that builds dashboards on top of
`soldr audit analyze`. Locking the shape here lets both sides ship
independently — soldr changes its internal implementation without
breaking the consuming agent; the agent updates its parsing only on
explicit schema-version bumps.

## `soldr audit capabilities --json`

Reports what the soldr+zccache pair supports on this host. Output is
a single JSON object on stdout. Exit code `0` on success.

```jsonc
{
  "schema": "soldr.audit.v1",
  "schema_version": 1,
  "embedded_zccache": {
    "available": true,
    "default_mode": "normal",
    "supported_modes": ["off", "summary", "normal", "verbose", "forensic"],
    "supported_sink_policies": ["block", "drop_low_priority", "degrade", "fail_lossless"]
  },
  "event_categories": [
    "host.lifecycle", "host.plan", "host.execute", "host.scheduler",
    "host.process",
    "zccache.compile", "zccache.cache", "zccache.depgraph",
    "zccache.artifact", "zccache.compiler_exec", "zccache.runtime",
    "system"
  ],
  "output_formats": ["jsonl"],
  "tokio_console": {
    "available": true,
    "default_bind": "127.0.0.1:6669"
  },
  "profile_modes": [
    {"name": "ai-perf",      "description": "Maximum verbosity + Tokio Console attach"},
    {"name": "ci-summary",   "description": "Summary mode + fail_lossless"},
    {"name": "default",      "description": "Normal mode + fail_lossless"}
  ]
}
```

Required keys for v1: `schema`, `schema_version`, `embedded_zccache`,
`event_categories`, `output_formats`. Everything else is additive.

## `soldr audit run`

Runs a host subcommand under audit and captures the artifacts into a
stable directory.

```text
soldr audit run \
  --profile ai-perf \
  --output <audit-run-dir> \
  --events soldr.*,zccache.*,runtime.tokio \
  --zccache embedded \
  --tokio-console localhost:1234 \
  -- build --release -p my-crate
```

### Required outputs in `<audit-run-dir>`

After the run exits, the operator finds these paths populated:

| File | Contents |
|---|---|
| `manifest.json` | The run manifest (shape below). The canonical entry point for any consumer. |
| `audit.jsonl` | The full event stream as written by zccache#926's writer. |
| `summary.json` | Aggregated run summary (counts, durations, top-N expensive compiles). |
| `zccache-journal.jsonl` | The compile-journal events written by zccache's compile pipeline. |
| `trace.json` | Tokio Console trace export (when `--tokio-console` was active). |
| `stdout.log` / `stderr.log` | The subcommand's I/O. |

### `manifest.json` shape

```jsonc
{
  "schema": "soldr.audit.v1",
  "schema_version": 1,
  "run_id": "01HQK4ZJVXJ2W6BX5R5VVZGN6Y",
  "started_at_ms": 1719423712000,
  "finished_at_ms": 1719423879000,
  "exit_code": 0,
  "subcommand": ["build", "--release", "-p", "my-crate"],
  "profile": "ai-perf",
  "outputs": {
    "audit_log":         "audit.jsonl",
    "summary":           "summary.json",
    "zccache_journal":   "zccache-journal.jsonl",
    "tokio_trace":       "trace.json",
    "stdout":            "stdout.log",
    "stderr":            "stderr.log"
  },
  "embedded_zccache": {
    "active": true,
    "mode": "verbose",
    "sink_policy": "fail_lossless"
  },
  "host_identity": {
    "product": "soldr",
    "instance_id": "<blake3-32-hex>",
    "workspace_id": "<blake3-32-hex>"
  }
}
```

Required keys: `schema`, `schema_version`, `run_id`, `started_at_ms`,
`exit_code`, `outputs`. The `finished_at_ms` is optional only because a
run cancelled by SIGINT may write the manifest before exit clock fires
— consumers should treat its absence as "use `audit.jsonl`'s last
event timestamp."

### Exit codes

| Code | Meaning |
|---|---|
| `0` | Subcommand exited 0; audit captured fully. |
| `subcommand's exit code` | Subcommand exited non-zero; audit captured fully. The audit infrastructure does not mask the underlying failure. |
| `64` | Audit setup failed before the subcommand started (output dir not writable, embedded zccache failed to start, …). The subcommand never ran. |
| `65` | Subcommand ran but audit capture was incomplete (writer disconnect mid-run). `manifest.json` records the partial state. |

`64` and `65` are reserved across all soldr releases for "audit
infrastructure problems"; consumers MUST treat them as terminal and
NOT retry without operator intervention.

## `soldr audit analyze`

Reads an audit-run-dir and emits a structured summary on stdout.

```text
soldr audit analyze <audit-run-dir> --json
```

### Output shape

```jsonc
{
  "schema": "soldr.audit.v1",
  "schema_version": 1,
  "run_id": "01HQK4ZJVXJ2W6BX5R5VVZGN6Y",
  "totals": {
    "wall_clock_ms": 167432,
    "compiles": 152,
    "cache_hits": 89,
    "cache_misses": 63,
    "cache_errors": 0,
    "bytes_written": 487523600
  },
  "phase_costs_ms": {
    "compile_exec": 142000,
    "cache_lookup": 1200,
    "cache_store": 18000,
    "depgraph_check": 4200,
    "depgraph_update": 1800
  },
  "scheduler_costs_ms": {
    "queue_wait_p50": 4,
    "queue_wait_p99": 187,
    "spawn_overhead_p50": 12
  },
  "concurrency": {
    "max_compile_in_flight":   8,
    "max_blocking_pool_in_flight": 16,
    "saturated_intervals_ms": 42000,
    "auto_priority_demotions": 47
  },
  "top_compiles_by_duration": [
    {"crate": "serde_derive", "duration_ms": 4287, "cache_outcome": "miss",
     "evidence_event_ids": ["evt-0001", "evt-0017"]},
    {"crate": "regex",        "duration_ms": 3812, "cache_outcome": "miss",
     "evidence_event_ids": ["evt-0042", "evt-0051"]}
  ],
  "recommendations": [
    {
      "id": "soldr.audit.rec.cache_miss_rate_high",
      "severity": "warn",
      "summary": "Cache miss rate 41% — investigate workspace fingerprint stability",
      "evidence_event_ids": ["evt-0001", "evt-0017", "evt-0042"]
    }
  ]
}
```

### Required keys for v1

- `schema`, `schema_version`, `run_id`, `totals`, `phase_costs_ms`,
  `scheduler_costs_ms`, `concurrency`, `top_compiles_by_duration`,
  `recommendations`.

`recommendations[].evidence_event_ids` MUST reference event IDs that
exist in `audit.jsonl`. An empty `recommendations` array is valid
("the run looked healthy, no findings"). Each recommendation `id`
follows the namespacing convention `soldr.audit.rec.<rule_name>` so
agents can subscribe to specific rules across runs.

## Versioning

This contract is bound to `schema_version: 1` of the
`soldr.audit.v1` identifier. Compatible-additive changes (new
optional keys, new event categories, new recommendation rule names)
land within v1 and consumers MUST ignore unknown keys. Breaking
changes (removed fields, renamed keys, changed semantics) bump the
schema version and the identifier to `soldr.audit.v2`; consumers MAY
support both side-by-side during a transition window.

## What this contract is NOT

- It is **not** a general-purpose tracing pipeline. The
  `runtime.tokio` events come from the embedded service's tokio
  runtime; deeper trace data lives in the `tokio_trace` export. The
  audit pipeline is for semantic build milestones, not per-syscall
  granularity.
- It is **not** a perf-cluster substitute. The perf-cluster Linux
  Docker harness ([`bench/`](../../bench)) remains the authoritative
  source for before/after performance numbers; the operator API is
  for in-run, AI/operator-facing diagnostics.
- It is **not** a billing or SLO data source. Hosts that need durable
  contract-compliant logging build that on top of this; the
  embedded contract does not claim WORM storage, signing, or
  long-retention guarantees.

## Cross-references

- [`audit-schema.md`](audit-schema.md) — the wire format the
  capabilities, manifest, and analyze outputs all derive from.
- [`embedded-service.md`](embedded-service.md) — the embedded zccache
  service contract the writer hooks into.
- [`vendored-hotfix-workflow.md`](vendored-hotfix-workflow.md) — the
  loop that contributes new fixtures back into the schema's test
  surface.
- zccache#926 — the writer that produces the JSONL `audit run`
  captures.
- soldr#977 Phase 4 — the soldr-side implementation of the three
  commands described here.
- zccache#929 — the embedded-service umbrella meta.

# Embedded Service Architecture

This document defines the design contract for embedding zccache inside host
daemons such as soldr and fbuild. It is the design-phase output for
zccache#903 and the implementation anchor for zccache#904 through zccache#910.

## Problem

zccache currently works well as a standalone drop-in compiler wrapper, but
products that use it as infrastructure have different needs than ordinary
per-user CLI usage. soldr, fbuild, and fastled can all invoke zccache as an
sccache-compatible tool, but doing so pushes them toward shared daemon state:

- one global daemon namespace per user or version,
- shared broker coordination,
- shared cache-root assumptions,
- shared lifecycle and shutdown behavior,
- shared runtime contention between unrelated products.

Versioned daemon names and the running-process broker reduce some conflicts,
but they still put unrelated products into a global coordination model. soldr
and fbuild are long-running build daemons; for those hosts, zccache should be a
private in-process build-cache engine with direct audit continuity, not only an
external process discovered by global naming.

## Goals

- Provide an async embedded zccache service API for host daemons.
- Let each host own a private cache root, product identity, and audit run.
- Preserve causal audit continuity across host and zccache work.
- Make zccache's tracing/audit/runtime choices part of the embedded contract.
- Keep existing global daemon, private daemon, brokered daemon, CLI, and Python
  modes compatible.
- Support vendored zccache hotfix validation in soldr/fbuild before upstreaming.

## Non-Goals

- Do not make embedded mode a neutral plugin ABI for arbitrary runtimes.
- Do not require drop-in CLI users to adopt Tokio or tracing directly.
- Do not replace the process/global daemon mode.
- Do not make Tokio Console the durable audit source of truth.
- Do not design an RPC protocol first and then force embedded hosts through it.

## Integration Modes

| Mode | Owner | Isolation | Primary Use |
|---|---|---|---|
| Global daemon | zccache CLI | Per-user/global | Drop-in wrapper compatibility |
| Private daemon | Host process plus zccache daemon process | Host-selected namespace/cache | Process isolation where embedding is not available |
| Brokered daemon | running-process broker plus zccache daemon | Broker-selected backend | Global process coordination and migration |
| Embedded service | Host daemon | Host-owned cache/service instance | soldr/fbuild tight integration |

Embedded mode is the most opinionated mode. The host adopts zccache's async
and audit contract in exchange for direct integration.

## Embedded Contract

A host daemon that embeds zccache accepts these constraints:

- It runs zccache from an async Tokio context.
- It uses `tracing` spans/events for cross-crate observability.
- It passes an audit context on every build/session/compile request.
- It participates in zccache cancellation and graceful shutdown semantics.
- It provides product identity, cache identity, and audit output paths.
- It accepts zccache-owned cache engine internals.

The host may own the Tokio runtime. zccache owns the service tasks it spawns
inside that runtime and exposes explicit handles for flush, stats, and shutdown.

## Ownership Boundaries

| Area | Host Owns | zccache Owns |
|---|---|---|
| Product identity | Product name, instance id, workspace id | Validation of identity fields used by zccache |
| Runtime | Tokio runtime and top-level cancellation token | Child tasks, blocking-task policy, runtime instrumentation hooks |
| Cache | Root directory selection and namespace | Artifact store, metadata cache, depgraph, temp dirs under the cache root |
| Audit | Top-level run id, output directory, event sink | zccache child spans/events, compile journal, phase summaries |
| Lifecycle | Build begin/plan/execute/terminate | Service start, flush, stats, graceful shutdown, forced shutdown |
| Process execution | Host policy constraints and cancellation | Compiler/tool subprocess execution and capture semantics |
| Redaction | Host-level policy and allow/deny lists | zccache-specific redaction of compiler args, env, paths, and cache keys |

Any persistent write in embedded mode must be rooted under the host-provided
cache root or audit output root unless the design explicitly documents an
exception.

## API Sketch

The exact Rust API will be finalized in zccache#905, but the design should
converge on this shape:

```rust
pub struct ZccacheService {
    // opaque handle
}

pub struct ZccacheConfig {
    pub host: HostIdentity,
    pub cache_root: PathBuf,
    pub audit: AuditConfig,
    pub limits: ServiceLimits,
    pub runtime: RuntimeHooks,
}

pub struct HostIdentity {
    pub product: String,
    pub instance_id: String,
    pub workspace_id: String,
}

pub struct AuditContext {
    pub run_id: String,
    pub trace_id: String,
    pub parent_span_id: Option<String>,
    pub command_id: Option<String>,
    pub session_id: Option<String>,
}

pub struct CompileRequest {
    pub audit: AuditContext,
    pub compiler: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

pub struct CompileResponse {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub cached: bool,
    pub cache_outcome: CacheOutcome,
    pub compile_id: String,
}

impl ZccacheService {
    pub async fn start(config: ZccacheConfig) -> Result<Self>;
    pub async fn compile(&self, request: CompileRequest) -> Result<CompileResponse>;
    pub async fn stats(&self) -> Result<ServiceStats>;
    pub async fn flush(&self) -> Result<FlushReport>;
    pub async fn shutdown(self, mode: ShutdownMode) -> Result<ShutdownReport>;
}
```

The API should avoid exposing daemon IPC framing types as the primary embedded
contract. Process modes can be adapters over the same engine.

## Lifecycle

Embedded service lifecycle is explicit:

1. Host creates a top-level audit run and cache identity.
2. Host starts `ZccacheService` with `ZccacheConfig`.
3. Host calls service methods from build phases.
4. zccache emits child spans/events under host-provided audit contexts.
5. Host calls `flush()` before final analysis artifacts are read.
6. Host calls graceful shutdown during daemon termination.
7. zccache reports unflushed/dropped work if shutdown cannot complete cleanly.

Cancellation must be cooperative and observable. A cancelled build should
produce a terminal audit event with enough detail to distinguish:

- host cancellation,
- zccache timeout,
- compiler/tool timeout,
- forced shutdown,
- internal error.

## Sync and Blocking Bridge

Embedded APIs are async-first. Blocking compatibility belongs at adapter
boundaries:

- CLI wrapper mode may create or enter a runtime.
- Global/private daemon clients may bridge blocking process invocations into
  async IPC.
- Embedded soldr/fbuild integration should call the async service directly.

Any blocking bridge must define:

- whether it may create a runtime,
- whether it may enter an existing runtime,
- how it avoids nested-runtime deadlocks,
- which blocking process or pipe operations run on `spawn_blocking`,
- which operations have watchdog timers,
- how timeout/cancellation is surfaced in audit events.

Windows pipe/process behavior is a first-class risk. No bridge may perform an
unbounded blocking IPC or process wait without a timeout and a diagnostic event.

## Audit Model

Durable structured audit events are the source of truth. Profilers are lenses
over runtime behavior.

The host owns the top-level trace:

```text
soldr.run
  soldr.begin
  soldr.plan
  soldr.execute
    soldr.command
      zccache.compile
        zccache.fingerprint
        zccache.cache_lookup
        zccache.depgraph
        zccache.compiler_exec
        zccache.artifact_store
  soldr.terminate
```

zccache joins the host trace by accepting `AuditContext` and/or by emitting
events inside the current `tracing` span.

### Event Shape

The concrete schema belongs to zccache#906. The intended base shape is:

```json
{
  "ts": "2026-06-23T12:00:00.123Z",
  "schema": "soldr.audit.v1",
  "run_id": "...",
  "trace_id": "...",
  "span_id": "...",
  "parent_span_id": "...",
  "category": "zccache.compile",
  "event": "compile.finished",
  "level": "info",
  "duration_ns": 123456789,
  "fields": {}
}
```

### Event Categories

Initial categories:

- `soldr.lifecycle`
- `soldr.plan`
- `soldr.execute`
- `soldr.scheduler`
- `soldr.process`
- `soldr.cache`
- `fbuild.lifecycle`
- `fbuild.plan`
- `fbuild.execute`
- `zccache.session`
- `zccache.compile`
- `zccache.cache_lookup`
- `zccache.depgraph`
- `zccache.artifact_store`
- `zccache.compiler_exec`
- `zccache.ipc`
- `runtime.tokio`
- `system.io`
- `system.cpu`

### Event Types

Initial event types:

- `run.started`
- `run.finished`
- `phase.started`
- `phase.finished`
- `target.planned`
- `command.started`
- `command.finished`
- `compile.started`
- `compile.finished`
- `cache.lookup`
- `cache.hit`
- `cache.miss`
- `cache.store`
- `depgraph.check`
- `depgraph.update`
- `process.spawn`
- `process.exit`
- `resource.wait`
- `runtime.task.blocked`

## Operator API

The operator API is tracked separately in zccache#910 because it spans soldr,
zccache, and eventually fbuild. The design requires three surfaces rather than
one overloaded "profile" command.

### Capability Discovery

```text
soldr audit capabilities --json
```

Example:

```json
{
  "schema_version": "1",
  "supports": {
    "event_log": true,
    "tokio_console": true,
    "zccache_embedded": true,
    "phase_summary": true,
    "artifact_exports": true
  },
  "event_categories": [
    "soldr.lifecycle",
    "soldr.plan",
    "soldr.execute",
    "soldr.process",
    "zccache.compile",
    "zccache.cache_lookup",
    "zccache.depgraph",
    "zccache.artifact_store",
    "runtime.tokio"
  ],
  "outputs": {
    "audit_jsonl": true,
    "trace_chrome": true,
    "summary_json": true,
    "tokio_console_bind": true
  }
}
```

### Audited Run

```text
soldr audit run \
  --profile ai-perf \
  --output .soldr/audit/runs/2026-06-23T120000Z \
  --events soldr.*,zccache.*,runtime.tokio \
  --zccache embedded \
  --tokio-console localhost:1234 \
  -- build ...
```

The command must return or write a manifest instead of requiring terminal
scraping.

### Post-Run Analysis

```text
soldr audit analyze .soldr/audit/runs/<id> --json
```

The analysis should answer:

- total wall time,
- begin/plan/execute/terminate timing,
- slowest targets,
- slowest compiler invocations,
- cache hit/miss/cached-error breakdown,
- zccache phase totals,
- top miss reasons,
- dependency graph costs,
- process spawn costs,
- idle/wait time,
- concurrency saturation,
- probable improvements with evidence event IDs.

## Audit Artifacts

An audited run should produce a manifest like:

```json
{
  "summary": "summary.json",
  "events": "audit.jsonl",
  "zccache_journal": "zccache-journal.jsonl",
  "trace": "trace.json",
  "tokio_console": "tokio-console.addr",
  "artifacts": "artifacts/"
}
```

`summary.json` is the first file an agent should read. `audit.jsonl` is the
causal event stream. `zccache-journal.jsonl` is the compile/cache-specific
evidence. `trace.json` is for timeline visualization. `tokio-console.addr` is
optional and only present when live runtime profiling is enabled.

## Finding Schema

Agent/operator recommendations should be machine-readable:

```json
{
  "finding_id": "perf.zccache.miss.compiler_exec",
  "severity": "medium",
  "confidence": 0.82,
  "evidence_event_ids": ["..."],
  "estimated_impact": {
    "wall_time_ms": 1200,
    "scope": "this run"
  },
  "suggested_action": "Investigate repeated cache misses caused by input_changed",
  "needs_reproduction": false
}
```

The finding schema must never rely on prose-only diagnostics. Every
recommendation should link back to event IDs or artifact paths.

## Profiling vs Auditing

Event tracing comes first. Profiling answers "where did time go"; audit tracing
answers "what happened and why." The embedded design needs both:

- durable audit JSONL for causal reconstruction,
- zccache compile journal for compile/cache details,
- phase summaries for aggregate counters,
- Tokio Console for live runtime symptoms such as blocked tasks, long polls,
  busy resources, and async contention.

Tokio Console is not a total build audit system. It is an attached microscope.

## Audit Modes

Initial modes:

| Mode | Intended Use |
|---|---|
| `off` | No durable audit beyond existing logs |
| `summary` | Final summary and aggregate counters |
| `normal` | Durable causal event log suitable for agent analysis |
| `verbose` | More fields and sub-phase events |
| `forensic` | Maximum detail for reproducing subtle integration bugs |

The schema must record the active mode. Higher modes may add fields/events but
must not remove required fields from lower modes.

## Backpressure and Failure Policy

The audit sink must not silently corrupt the build's explanation. If the sink
falls behind or disk fills, zccache/host integration must choose an explicit
policy and report it:

- block until audit catches up,
- drop low-priority events and increment `audit_lost_events`,
- degrade from verbose/forensic to normal/summary,
- fail the build when the selected mode requires lossless audit.

The selected policy is part of `AuditConfig`.

## Security and Redaction

Audit events can expose secrets. The design must treat redaction as a contract,
not a formatter detail.

Sensitive inputs include:

- environment variables,
- compiler arguments,
- include paths,
- repository paths,
- auth tokens,
- private dependency URLs,
- command stdout/stderr.

Redaction should be deterministic and testable. Events should preserve enough
shape to diagnose behavior without leaking raw values.

## Vendored Hotfix Workflow

The vendoring workflow is tracked in zccache#909. Embedded integrations may
vendor zccache into soldr/fbuild to validate hotfixes against real workloads:

```text
vendor zccache into soldr/fbuild
  -> patch embedded integration locally
  -> validate against real host workloads and audit traces
  -> upstream the proven zccache fix
  -> update the vendored pin/dependency after merge/release
```

Upstream zccache PRs should carry evidence from the host audit artifacts when
the bug only reproduces in the host integration.

## Rollout Plan

1. Land this design document.
2. Define the audit operator API contract (zccache#910).
3. Define the durable audit schema and fixtures (zccache#906).
4. Extract or introduce the embedded zccache service API (zccache#905).
5. Build the first soldr embedded integration (zccache#907).
6. Feed integration gaps back into zccache API/audit fixes.
7. Build the fbuild embedded integration (zccache#908).
8. Document and exercise the vendored hotfix workflow (zccache#909).

## Open Questions

- Which crate should expose the public embedded API surface?
- Should `AuditContext` be a zccache-owned type, a shared host type, or a
  small compatibility layer around `tracing` span context?
- Which audit mode should soldr use by default for local builds?
- Which audit failures should fail the build versus degrade the audit?
- How much command stdout/stderr should be captured by default?
- Should zccache produce Chrome trace output directly or emit enough events for
  the host to derive it?
- What is the minimum host runtime contract for fbuild if its daemon differs
  from soldr's runtime model?

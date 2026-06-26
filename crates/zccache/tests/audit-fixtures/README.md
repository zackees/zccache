# Audit-trace fixtures

Canonical JSONL fixtures referenced by
[`docs/architecture/audit-schema.md`](../../../docs/architecture/audit-schema.md).
Each line is a single audit event matching the `soldr.audit.v1` schema
defined in [`crates/zccache/src/audit.rs`](../../src/audit.rs).

## Normalization

The fixtures are **normalized**: all wall-clock fields are zeroed, all
identifiers are deterministic stand-ins (`run-fixture`, `trace-fixture`,
…), and host-specific paths are replaced with `<workspace>/...` so the
fixtures can be diffed across runs and across operating systems.

The corresponding fixture test (lands with the writer in zccache#926)
applies the same normalizer to a captured embedded-service run and
asserts byte-for-byte equality against the fixture.

## Files

| File | Captures |
|---|---|
| `embedded-cold-compile.jsonl` | Single rustc invocation, cold cache (miss → store). |
| `embedded-warm-compile.jsonl` | Same invocation re-run against a warm cache (hit). |
| `embedded-cancelled-compile.jsonl` | A compile aborted via `AuditContext` cancellation (zccache#923). |

## Contributing new fixtures

Host products that surface a new validation scenario during the
[vendored-hotfix-workflow](../../../docs/architecture/vendored-hotfix-workflow.md)
contribute the captured trace back here under the naming convention
`embedded-<scenario>.jsonl`. The fixture's first event MUST be the
`host.lifecycle.run.started` that opens the scope, and the last event
MUST be the corresponding `host.lifecycle.run.finished` (or
`run.cancelled`).

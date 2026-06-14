# Wire stability contract

> The zccache wire protocol is a frozen, append-only schema. Any client built against
> a v1 daemon must keep working against every future v1 daemon, and vice versa.

This document is the social-contract side of zackees/zccache#693 Phase 1. The
machine-enforcement side is `ci/check_wire_stability.py` + the snapshot at
`ci/wire_stability_snapshot.txt`, run on every PR via
`.github/workflows/wire-stability.yml`.

## What is the wire

The zccache wire is the byte sequence exchanged between the daemon and any
in-process or out-of-process client:

- The outer envelope. On the modern lane (`ZCCACHE_DAEMON_WIRE=frame`), this is
  the running-process broker envelope `[u8 envelope_version=1][u32 LE
  body_len][prost broker_v1::Frame]` with `payload_protocol =
  ZCCACHE_FRAME_PAYLOAD_PROTOCOL = 0x7A63`. Outer envelope stability is
  inherited from `running-process` (see its registry + framing modules); we
  pin our payload-protocol id with `running_process::register_payload_protocol!`
  so the SDK's compile-time collision checks fire on every build.
- The inner payload. Prost-encoded `zccache.v1.Request` / `zccache.v1.Response`
  messages (plus their transitive types) defined in
  [`crates/zccache/proto/zccache_v1.proto`](../crates/zccache/proto/zccache_v1.proto)
  and the auxiliary
  [`crates/zccache/src/artifact/rust_plan_manifest.proto`](../crates/zccache/src/artifact/rust_plan_manifest.proto).
  These messages are the focus of the rest of this document.

## The contract

For every field that has ever shipped on the wire:

1. **Its field number never changes.** Renumbering a field is a wire break —
   old clients and new daemons cannot agree on what bytes mean. The
   `(message, field_number)` pair is the durable identifier.
2. **Its type never changes.** `string` → `bytes`, `int32` → `uint32`,
   `optional` → `required` (or vice versa under proto3 semantics) — all
   prohibited. The protobuf wire is not type-erased; readers depend on the
   declared type.
3. **Its name never changes.** Field names are not on the wire for the
   common case but they are part of the public API (codegen output) and
   downstream code reads them. We treat renames as breaking.
4. **It is never removed.** Deprecation is fine (rename to `deprecated_*`
   with a comment); deletion is not. The CI guard rejects the deletion.
5. **Enum value names + numbers never change** for the same reasons.

What **is** allowed without bumping the protocol version:

- **Adding new fields** to existing messages with previously-unused field
  numbers. Old readers will skip the unknown field and continue. Forward-
  compatible by protobuf's wire semantics.
- **Adding new messages and enums.**
- **Adding new oneof variants** to existing `oneof`s, again using
  previously-unused field numbers. Old readers see the unknown variant as
  "not set" and should treat it as a generic error.
- **Documentation changes** (comments in the `.proto`).

## Versioning

The package is `zccache.v1` and the file's frozen body lives at
[`crates/zccache/proto/zccache_v1.proto`](../crates/zccache/proto/zccache_v1.proto).
A future incompatible change — if one is ever needed — would ship as a
parallel `zccache.v2` namespace with its own `.proto`, never as an in-place
mutation of v1. Old daemons keep speaking v1 forever; clients that need v2
features negotiate v2 explicitly. This mirrors D-Bus's wire-stability
posture and Docker's API-version negotiation; see #693 for context and
follow-up issues for the handshake (#693 Phases 2–4).

## How the CI guard works

`ci/check_wire_stability.py` extracts every `(message_or_enum, field_number)
→ (field_name, field_type)` tuple from the proto files and compares the
result against `ci/wire_stability_snapshot.txt`. The script:

- **Fails** if any snapshot entry is missing or has a different
  `(name, type)` in the current proto — that's a removal, rename, or
  type-change.
- **Succeeds** if the snapshot is a subset of the current proto — adding
  new fields, messages, enums, or oneof variants does not require touching
  the snapshot.
- **Always re-parses both `.proto` files end-to-end.** Syntax it does not
  model is treated as a hard parse error rather than silently skipped;
  bias is toward failing closed.

If a wire change is intentional and you understand its compatibility
impact (i.e. you're cutting a `v2` lane, or rotating a deprecated field
that no shipped client has read), regenerate the snapshot:

```
uv run python ci/check_wire_stability.py --write-snapshot
```

Commit the snapshot delta in the same PR as the proto change and explain
the bump in the commit message. The PR description should call out which
of the rules above is being relaxed and why.

## Scope

This document covers the **prost-encoded payload** layer. The outer broker
envelope (running-process `Frame`) is governed by
[zackees/running-process](https://github.com/zackees/running-process) and
its own stability docs; zccache pins the payload-protocol id and the
exact construction recipe in `crates/zccache/src/protocol/wire_frame.rs`
via the `register_payload_protocol!` macro. A change to the outer envelope
would require a coordinated wire bump there and a parallel `v2` lane here.

## References

- zackees/zccache#693 — the design issue this document closes Phase 1 of.
- zackees/zccache#735 — meta tracker for the in-flight PR burn-down.
- zackees/running-process registry — first-party + registered consumer
  payload-protocol ids (`ZCCACHE_PAYLOAD_PROTOCOL = 0x7A63`).
- [D-Bus specification — wire stability](https://dbus.freedesktop.org/doc/dbus-specification.html)
- [Docker Engine API version negotiation](https://docs.docker.com/reference/api/engine/)
- [sccache version-mismatch anti-pattern](https://github.com/Mozilla-Actions/sccache-action/issues/171)
  — the user-experience case we are avoiding.

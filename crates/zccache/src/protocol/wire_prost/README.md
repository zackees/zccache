# wire_prost

Prost (protobuf) wire helpers and v16 dispatcher scaffolding for the zccache
daemon IPC protocol.

The public path `protocol::wire_prost::<Name>` is preserved by re-exports from
`mod.rs`; callers do not need to know about the submodule split.

## Files

- `mod.rs` — public API surface: `WireFormat`, `ClientWireSelection`, env
  parsing helpers, re-exports of the submodule entry points, and the generated
  `zccache_v1` protobuf schema module.
- `api.rs` — narrow daemon-control / maintenance request and response
  converters (`supported_control_*`), `default_request_id`,
  `full_family_wire_format_from_env`, and `response_from_decoded_wire`.
- `request.rs` — `request_to_prost` / `request_from_prost`: full conversion
  between the internal `Request` enum and the v16 prost schema.
- `response.rs` — `response_to_prost` / `response_from_prost`: full conversion
  between the internal `Response` enum and the v16 prost schema.
- `convert.rs` — shared field-level conversion helpers (paths, env pairs,
  artifacts, lookup/store results, exec output streams + cache policy,
  session stats, phase profile, daemon status, private daemon options).
- `frame.rs` — length-prefixed v16 prost frame encoder/decoder
  (`encode_prost_message` / `decode_prost_message`).

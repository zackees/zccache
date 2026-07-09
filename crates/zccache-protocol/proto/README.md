# proto

Protobuf schema sources for `zccache-protocol`.

`build.rs` compiles `zccache_v1.proto` with `prost-build` and the vendored
`protoc` binary. Generated Rust is included by `src/wire_prost/mod.rs`.

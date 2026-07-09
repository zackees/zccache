# zccache-protocol

Internal, unpublished crate for zccache daemon protocol types and wire codecs.

The protobuf schema in `proto/zccache_v1.proto` is compiled by `build.rs`.
The generated module remains available through `wire_prost::zccache_v1`, which
includes `concat!(env!("OUT_DIR"), "/zccache.v1.rs")`.

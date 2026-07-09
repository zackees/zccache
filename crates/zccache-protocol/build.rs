// Build scripts are allowed to panic on setup failure: cargo surfaces
// the panic message and fails the build cleanly. Each expect() below
// encodes a build-time invariant.
#![allow(clippy::expect_used)]

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=proto/zccache_v1.proto");

    let protoc = protoc_bin_vendored::protoc_bin_path()
        .expect("vendored protoc is available for zccache wire protobufs");
    std::env::set_var("PROTOC", protoc);

    prost_build::Config::new()
        .compile_protos(&["proto/zccache_v1.proto"], &["proto"])
        .expect("zccache wire protobufs compile");
}

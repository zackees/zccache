// Build scripts are allowed to panic on setup failure: cargo surfaces
// the panic message and fails the build cleanly. Per-site `#[expect]`
// would add noise for no gain — every expect() in this file encodes a
// real build-time invariant (TARGET set by cargo, vendored protoc
// present, protobuf compile succeeds).
#![allow(clippy::expect_used)]

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    compile_zccache_wire_proto();

    // Expose the build's target triple at runtime so `zccache symbols install`
    // can construct the matching GitHub Release asset URL without asking the
    // user to type it. `TARGET` is set by cargo for build scripts.
    let target = std::env::var("TARGET").expect("cargo sets TARGET for build scripts");
    println!("cargo:rustc-env=ZCCACHE_BUILD_TARGET={target}");
}

fn compile_zccache_wire_proto() {
    println!("cargo:rerun-if-changed=proto/zccache_v1.proto");

    let protoc = protoc_bin_vendored::protoc_bin_path()
        .expect("vendored protoc is available for zccache wire protobufs");
    std::env::set_var("PROTOC", protoc);

    prost_build::Config::new()
        .compile_protos(&["proto/zccache_v1.proto"], &["proto"])
        .expect("zccache wire protobufs compile");
}

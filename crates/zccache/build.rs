// Build scripts are allowed to panic on setup failure: cargo surfaces
// the panic message and fails the build cleanly. Per-site `#[expect]`
// would add noise for no gain — every expect() in this file encodes a
// real build-time invariant (TARGET set by cargo).
#![allow(clippy::expect_used)]

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // Expose the build's target triple at runtime so `zccache symbols install`
    // can construct the matching GitHub Release asset URL without asking the
    // user to type it. `TARGET` is set by cargo for build scripts.
    let target = std::env::var("TARGET").expect("cargo sets TARGET for build scripts");
    println!("cargo:rustc-env=ZCCACHE_BUILD_TARGET={target}");
}

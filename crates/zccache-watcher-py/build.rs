fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // This crate is the extension module unconditionally -- unlike the old
    // feature-gated build in `zccache-watcher`, there is no non-python mode.
    pyo3_build_config::add_extension_module_link_args();
}

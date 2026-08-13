// Every construct in this file must produce one enforce_platform_boundary
// error. See ui/README.md for why the shadow modules and cfg gates exist.

#![allow(dead_code, unused)]
#![cfg_attr(windows, allow(unused_imports))]

// Shadow modules stand in for the real native crates so the fixtures stay
// host-independent: the lint matches pre-expansion path names, so a local
// `mod libc` exercises the same check as the real extern crate without
// linking it. `std` itself cannot be shadowed (E0433), so the
// std::os::{windows,unix} cases below use the real std module gated on the
// cfg where each exists.
#[allow(non_camel_case_types)]
mod libc {
    pub type c_int = i32;
}
#[allow(non_camel_case_types, non_snake_case)]
mod windows_sys {
    pub mod Win32 {
        pub mod Foundation {
            pub type HANDLE = *mut u8;
        }
    }
}
mod platform_win {
    pub fn win_only() {}
}
mod platform_imp {
    pub fn some_leaf() {}
}

// 1. A private function with cfg!(windows).
fn private_cfg_macro() -> bool {
    cfg!(windows)
}

// 2. Private and public items with #[cfg(unix)].
#[cfg(unix)]
fn private_unix_item() {}
#[cfg(unix)]
pub fn public_unix_item() {}

// 3. #[cfg_attr(target_os = "windows", ...)].
#[cfg_attr(target_os = "windows", allow(dead_code))]
fn cfg_attr_windows_item() {}

// 4. target_arch / target_env / target_family variants.
#[cfg(target_arch = "x86_64")]
fn arch_item() {}
#[cfg(target_env = "msvc")]
fn env_item() {}
#[cfg(target_family = "unix")]
fn family_item() {}

// 5. Direct concrete-module and platform_imp references.
fn concrete_module_reference() {
    platform_win::win_only();
}
fn imp_reference() {
    platform_imp::some_leaf();
}

// 6. Imports from std::os::windows / std::os::unix / windows_sys / libc.
#[cfg(windows)]
use std::os::windows::io::RawHandle;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use libc::c_int;
use windows_sys::Win32::Foundation::HANDLE;

#[cfg(windows)]
fn use_windows_os() {
    let _: RawHandle = 0 as RawHandle;
}
#[cfg(unix)]
fn use_unix_os(meta: &std::fs::Metadata) -> u64 {
    meta.ino()
}
fn use_libc() {
    let _: c_int = 0;
}
fn use_windows_sys() {
    let _: HANDLE = std::ptr::null_mut();
}

fn main() {}

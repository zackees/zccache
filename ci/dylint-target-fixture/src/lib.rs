#![allow(dead_code)]

#[cfg(all(
    feature = "violate",
    target_os = "linux",
    target_env = "gnu",
    target_arch = "x86_64"
))]
fn linux_gnu_x64() {
    let _: std::path::PathBuf = std::path::PathBuf::new();
}

#[cfg(all(
    feature = "violate",
    target_os = "linux",
    target_env = "gnu",
    target_arch = "aarch64"
))]
fn linux_gnu_arm() {
    let _: std::path::PathBuf = std::path::PathBuf::new();
}

#[cfg(all(
    feature = "violate",
    target_os = "linux",
    target_env = "musl",
    target_arch = "x86_64"
))]
fn linux_musl_x64() {
    let _: std::path::PathBuf = std::path::PathBuf::new();
}

#[cfg(all(
    feature = "violate",
    target_os = "linux",
    target_env = "musl",
    target_arch = "aarch64"
))]
fn linux_musl_arm() {
    let _: std::path::PathBuf = std::path::PathBuf::new();
}

#[cfg(all(
    feature = "violate",
    target_os = "windows",
    target_env = "msvc",
    target_arch = "x86_64"
))]
fn windows_x64() {
    let _: std::path::PathBuf = std::path::PathBuf::new();
}

#[cfg(all(
    feature = "violate",
    target_os = "windows",
    target_env = "msvc",
    target_arch = "aarch64"
))]
fn windows_arm() {
    let _: std::path::PathBuf = std::path::PathBuf::new();
}

#[cfg(all(feature = "violate", target_os = "macos", target_arch = "x86_64"))]
fn macos_x64() {
    let _: std::path::PathBuf = std::path::PathBuf::new();
}

#[cfg(all(feature = "violate", target_os = "macos", target_arch = "aarch64"))]
fn macos_arm() {
    let _: std::path::PathBuf = std::path::PathBuf::new();
}

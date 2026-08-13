//! Linux host facts.

use std::path::PathBuf;

pub const fn os() -> &'static str { std::env::consts::OS }
pub const fn arch() -> &'static str { std::env::consts::ARCH }
pub fn home_dir() -> Option<PathBuf> { std::env::var_os("HOME").map(PathBuf::from) }
pub fn current_user() -> Option<String> { std::env::var("USER").ok() }
pub fn runtime_dir() -> Option<String> { std::env::var("XDG_RUNTIME_DIR").ok() }
pub const fn is_elevated() -> bool { true }
pub const fn defender_supported() -> bool { false }

pub fn cpu_identity_material() -> String {
    let mut material = format!("arch={}\0os={}", arch(), os());
    if let Ok(machine_id) = std::fs::read_to_string("/etc/machine-id") {
        let machine_id = machine_id.trim();
        if !machine_id.is_empty() { material.push_str("\0machine-id="); material.push_str(machine_id); }
    }
    append_host_or_pid(&mut material);
    append_cpu_features(&mut material);
    material
}

fn append_host_or_pid(material: &mut String) {
    if let Some(name) = std::env::var_os("HOSTNAME") {
        material.push_str("\0HOSTNAME="); material.push_str(&name.to_string_lossy());
    } else if !material.contains("machine-id=") {
        material.push_str("\0pid="); material.push_str(&std::process::id().to_string());
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn append_cpu_features(material: &mut String) {
    for (name, present) in [("sse2", std::arch::is_x86_feature_detected!("sse2")), ("sse4.2", std::arch::is_x86_feature_detected!("sse4.2")), ("avx", std::arch::is_x86_feature_detected!("avx")), ("avx2", std::arch::is_x86_feature_detected!("avx2")), ("avx512f", std::arch::is_x86_feature_detected!("avx512f")), ("fma", std::arch::is_x86_feature_detected!("fma")), ("bmi1", std::arch::is_x86_feature_detected!("bmi1")), ("bmi2", std::arch::is_x86_feature_detected!("bmi2"))] {
        if present { material.push_str("\0feature="); material.push_str(name); }
    }
}
#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
fn append_cpu_features(_: &mut String) {}

pub fn defender_exclusions() -> Result<Vec<PathBuf>, crate::host::DefenderError> {
    Err(crate::host::DefenderError::Unsupported)
}
pub fn add_defender_exclusion(_: &std::path::Path) -> Result<(), crate::host::DefenderError> {
    Err(crate::host::DefenderError::Unsupported)
}
pub fn remove_defender_exclusion(_: &std::path::Path) -> Result<(), crate::host::DefenderError> {
    Err(crate::host::DefenderError::Unsupported)
}

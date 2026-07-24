//! Host identity handling for compiler `native` CPU selections.
//!
//! `-march=native` and rustc's `-C target-cpu=native` are instructions to
//! expand the target from the *current host*, rather than stable command-line
//! values. Cache keys must therefore not be portable across arbitrary hosts
//! for an invocation that contains one. We deliberately salt such keys rather
//! than rewriting compiler argv: rewriting would require reproducing each
//! compiler's CPU expansion semantics and could change the actual compile.

use std::sync::OnceLock;

/// Returns the opaque, stable-on-this-host salt for `native` CPU selections.
///
/// The value is a hash, so machine-identifying inputs never enter a persisted
/// compilation context or cache key in clear text. It combines a host-local
/// identifier with architecture and observed CPU capabilities. If a platform
/// cannot expose a host identifier, the process id is a fail-closed fallback:
/// it gives up reuse after a daemon restart rather than allowing an unknown
/// host to share a `native` artifact.
#[must_use]
pub fn host_cpu_identity_salt() -> &'static str {
    static SALT: OnceLock<String> = OnceLock::new();
    SALT.get_or_init(|| host_cpu_identity_salt_from(&host_cpu_identity_material()))
}

/// Hashes supplied host identity material into the key-safe salt.
///
/// This is public specifically to make cross-host key behavior testable
/// without mocking global process state or relying on the test machine's CPU.
#[must_use]
pub fn host_cpu_identity_salt_from(identity: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"zccache-native-cpu-host-v1\0");
    hasher.update(identity.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// Whether a C/C++ key flag asks the compiler to select CPU settings locally.
#[must_use]
pub fn is_cxx_native_cpu_flag(flag: &str) -> bool {
    matches!(flag, "-march=native" | "-mtune=native" | "-mcpu=native")
}

/// Whether a rustc codegen setting asks rustc/LLVM to select the local CPU.
#[must_use]
pub fn is_rustc_native_cpu_flag(flag: &str) -> bool {
    matches!(flag, "target-cpu=native" | "target-feature=native")
}

fn host_cpu_identity_material() -> String {
    let mut material = format!(
        "arch={}\0os={}",
        std::env::consts::ARCH,
        std::env::consts::OS
    );

    // Linux exposes a per-install machine id, which avoids sharing a native
    // artifact even when two machines happen to report the same CPU model.
    // Keep the raw value local: the caller hashes all material before it can
    // become part of a cache key or snapshot.
    #[cfg(target_os = "linux")]
    if let Ok(machine_id) = std::fs::read_to_string("/etc/machine-id") {
        let machine_id = machine_id.trim();
        if !machine_id.is_empty() {
            material.push_str("\0machine-id=");
            material.push_str(machine_id);
        }
    }

    // Windows normally provides COMPUTERNAME and Unix systems commonly expose
    // HOSTNAME. This is intentionally only an additional discriminator; the
    // identity is already domain-separated and opaque after hashing.
    for variable in ["COMPUTERNAME", "HOSTNAME"] {
        if let Some(value) = std::env::var_os(variable) {
            material.push('\0');
            material.push_str(variable);
            material.push('=');
            material.push_str(&value.to_string_lossy());
        }
    }

    append_observed_cpu_features(&mut material);

    // A host without an exposed identifier must never collapse into a common
    // cross-host salt. PID costs only a restart hit in that rare fallback.
    if !material.contains("machine-id=")
        && !material.contains("COMPUTERNAME=")
        && !material.contains("HOSTNAME=")
    {
        material.push_str("\0pid=");
        material.push_str(&std::process::id().to_string());
    }

    material
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn append_observed_cpu_features(material: &mut String) {
    for (name, present) in [
        ("sse2", std::arch::is_x86_feature_detected!("sse2")),
        ("sse4.2", std::arch::is_x86_feature_detected!("sse4.2")),
        ("avx", std::arch::is_x86_feature_detected!("avx")),
        ("avx2", std::arch::is_x86_feature_detected!("avx2")),
        ("avx512f", std::arch::is_x86_feature_detected!("avx512f")),
        ("fma", std::arch::is_x86_feature_detected!("fma")),
        ("bmi1", std::arch::is_x86_feature_detected!("bmi1")),
        ("bmi2", std::arch::is_x86_feature_detected!("bmi2")),
    ] {
        if present {
            material.push_str("\0feature=");
            material.push_str(name);
        }
    }
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
fn append_observed_cpu_features(_: &mut String) {}

#[cfg(test)]
mod tests {
    use super::{host_cpu_identity_salt_from, is_cxx_native_cpu_flag, is_rustc_native_cpu_flag};

    #[test]
    fn synthetic_host_identity_is_stable_but_host_specific() {
        assert_eq!(
            host_cpu_identity_salt_from("host-a-avx2"),
            host_cpu_identity_salt_from("host-a-avx2")
        );
        assert_ne!(
            host_cpu_identity_salt_from("host-a-avx2"),
            host_cpu_identity_salt_from("host-b-sse2")
        );
    }

    #[test]
    fn native_flag_detection_is_exact() {
        for flag in ["-march=native", "-mtune=native", "-mcpu=native"] {
            assert!(is_cxx_native_cpu_flag(flag));
        }
        assert!(!is_cxx_native_cpu_flag("-march=x86-64-v3"));
        assert!(is_rustc_native_cpu_flag("target-cpu=native"));
        assert!(!is_rustc_native_cpu_flag("target-feature=+avx2"));
    }
}

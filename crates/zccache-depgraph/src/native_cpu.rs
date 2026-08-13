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
    crate::platform::host::cpu_identity_material()
}

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

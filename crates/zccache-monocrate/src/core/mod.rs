//! Core types and traits for zccache.
//!
//! This crate contains shared types, error definitions, path utilities,
//! and configuration structures used across all zccache crates.

pub mod config;
pub mod crash;
pub mod defender;
pub mod error;
pub mod lifecycle;
pub mod path;
pub mod version;

pub use error::{Error, Result};
pub use path::{normalize, normalize_for_key, normalize_msys_path, stable_path_id, NormalizedPath};

/// The version string from Cargo.toml (workspace version).
///
/// This is the single source of truth for the version that `zccache --version`
/// prints (via clap's `#[command(version)]`). The test below ensures it stays
/// in sync with `pyproject.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    /// Ensures the root pyproject.toml declares version as dynamic (derived
    /// from Cargo.toml at build time via setup.py). A hardcoded version would
    /// drift from the workspace version and cause release mismatches.
    #[test]
    fn pyproject_version_is_dynamic() {
        let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates/ dir")
            .parent()
            .expect("workspace root");

        let pyproject = std::fs::read_to_string(workspace_root.join("pyproject.toml"))
            .expect("failed to read pyproject.toml");

        assert!(
            pyproject
                .lines()
                .any(|line| line.trim().contains("dynamic") && line.contains("version")),
            "pyproject.toml must use dynamic = [\"version\"] (derived from Cargo.toml)"
        );
        assert!(
            !pyproject.lines().any(|line| {
                let t = line.trim();
                t.starts_with("version")
                    && t.contains('=')
                    && t.contains('"')
                    && !t.contains("dynamic")
            }),
            "pyproject.toml must not have a hardcoded version field"
        );
    }
}

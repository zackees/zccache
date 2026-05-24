//! Release-build marker and per-crash sidecar primitives.
//!
//! See [`self::marker`] for the 128-byte footer format that
//! distinguishes a release-built `zccache.exe` from a local dev build,
//! and [`self::cache`] for the `<cache>/symbols/<v>-<triple>/` layout
//! and the `<dump>.symref` sidecar that points each crash dump at its
//! shared symbol directory.
//!
//! The actual symbol-archive *fetch* lives in `zccache-cli::symbols`
//! (cross-process locked, atomic-renamed, archive-cached). This crate
//! deliberately doesn't duplicate that machinery.

pub mod cache;
pub mod marker;

pub use cache::{is_ready, mark_ready, symbols_dir_for, write_symref_sidecar, READY_SENTINEL};
pub use marker::{read_marker_from_current_exe, read_marker_from_path, ReleaseMarker};

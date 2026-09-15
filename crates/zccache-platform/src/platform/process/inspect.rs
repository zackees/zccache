//! Process liveness, image-path, CPU-tick, and peak-memory inspection.

use crate::platform_imp;

#[must_use]
pub fn is_alive(pid: u32) -> bool {
    platform_imp::process::inspect::is_alive(pid)
}

pub fn executable_path(pid: u32) -> Option<std::path::PathBuf> {
    platform_imp::process::inspect::executable_path(pid)
}

pub fn cpu_ticks(pid: u32) -> Option<u64> {
    platform_imp::process::inspect::cpu_ticks(pid)
}

/// Resident-memory high-water mark of `pid`, in bytes (soldr#3152).
///
/// Linux reads `VmHWM`, macOS the lifetime maximum physical footprint, and
/// Windows `PeakWorkingSetSize`. `None` when the process is gone or its
/// memory accounting has already been torn down (a Unix zombie).
pub fn peak_rss_bytes(pid: u32) -> Option<u64> {
    platform_imp::process::inspect::peak_rss_bytes(pid)
}

/// Current resident bytes of `pid` plus every live descendant (zccache#1588).
///
/// The per-process figure is current resident memory, not a high-water mark:
/// Linux `VmRSS`, macOS `ri_resident_size`, Windows `WorkingSetSize`.
/// Descendants come from `/proc/<pid>/task/*/children` on Linux,
/// `proc_listchildpids` on macOS and a Toolhelp32 snapshot on Windows, so a
/// caller that samples this repeatedly and keeps the maximum gets a sampled
/// peak for the whole tree. That is the only way the memory of a `rustc` ->
/// `cc` -> `ld` link shows up: the linker is a grandchild. `None` when `pid`
/// itself cannot be read; the walk is capped at [`MAX_TREE_PROCESSES`].
pub fn tree_rss_bytes(pid: u32) -> Option<u64> {
    platform_imp::process::inspect::tree_rss_bytes(pid)
}

/// Upper bound on processes [`tree_rss_bytes`] visits in one call, so a fork
/// bomb under a compiler cannot turn one sample into an unbounded walk.
pub const MAX_TREE_PROCESSES: usize = platform_imp::process::inspect::MAX_TREE_PROCESSES;

/// Whether [`peak_rss_bytes`] stays exact for a child that has exited but
/// whose handle is still held. True on Windows, where the retained process
/// handle keeps the pid and its final peak readable. False on Unix, where the
/// reaped child's memory accounting is gone and its pid may be reused.
pub const PEAK_RSS_READABLE_AFTER_EXIT: bool =
    platform_imp::process::inspect::PEAK_RSS_READABLE_AFTER_EXIT;

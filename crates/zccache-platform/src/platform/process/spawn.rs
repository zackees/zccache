//! Native spawn and ownership mechanics.

/// Spawn a disposable sleeping child. Used by cross-platform characterization
/// tests; callers own and must reap the returned child.
pub fn sleeping_child(duration: std::time::Duration) -> std::io::Result<std::process::Child> {
    crate::platform_imp::process::spawn::sleeping_child(duration)
}

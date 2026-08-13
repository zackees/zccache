//! Native spawn and ownership mechanics.

/// Spawn a disposable sleeping child. Used by cross-platform characterization
/// tests; callers own and must reap the returned child.
pub fn sleeping_child(duration: std::time::Duration) -> std::io::Result<std::process::Child> {
    crate::platform_imp::process::spawn::sleeping_child(duration)
}

/// Attach a daemon-owned child to the host's owner-death primitive.
///
/// Windows uses a process-wide kill-on-close Job Object. Unix ownership is
/// configured before spawn, so this post-spawn operation is a no-op there.
pub fn attach_owner_death(child: &tokio::process::Child) -> std::io::Result<()> {
    crate::platform_imp::process::spawn::attach_owner_death(child)
}

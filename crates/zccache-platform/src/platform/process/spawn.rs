//! Native spawn and ownership mechanics.

/// Spawn a disposable sleeping child. Used by cross-platform characterization
/// tests; callers own and must reap the returned child.
pub fn sleeping_child(duration: std::time::Duration) -> std::io::Result<std::process::Child> {
    crate::platform_imp::process::spawn::sleeping_child(duration)
}

/// Spawn a disposable child and capture a marker from its standard output.
pub fn echo_output(marker: &str) -> std::io::Result<std::process::Output> {
    crate::platform_imp::process::spawn::echo_output(marker)
}

/// Attach a daemon-owned child to the host's post-spawn owner-death primitive.
///
/// Windows uses zccache's process-wide kill-on-close Job Object. Linux and
/// macOS configure ownership before spawn, so this is a no-op there.
pub fn attach_owner_death(child: &tokio::process::Child) -> std::io::Result<()> {
    crate::platform_imp::process::spawn::attach_owner_death(child)
}

/// Whether owner-death containment is installed by running-process before the
/// child can execute.
#[must_use]
pub fn uses_pre_spawn_owner_death() -> bool {
    crate::platform_imp::process::spawn::uses_pre_spawn_owner_death()
}

/// Run a CLI entry point with the host's required stack reservation.
pub fn run_cli_entry(entry: fn() -> std::process::ExitCode) -> std::process::ExitCode {
    crate::platform_imp::process::spawn::run_cli_entry(entry)
}

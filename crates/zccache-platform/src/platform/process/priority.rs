//! Neutral priority classes and native application.

/// Host-neutral scheduling priority ordered from normal to most deprioritized.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Priority {
    High,
    Normal,
    Low,
    Idle,
}

/// Apply `priority` to an already-spawned child.
///
/// The concrete host implementation uses the child's existing PID or native
/// handle, so this does not add a process lookup to the spawn hot path.
pub fn apply_to_child(child: &tokio::process::Child, priority: Priority) -> std::io::Result<()> {
    crate::platform_imp::process::priority::apply_to_child(child, priority)
}

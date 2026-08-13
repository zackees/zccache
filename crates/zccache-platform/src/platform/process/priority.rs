//! Neutral priority classes and native application.

/// Host-neutral scheduling priority ordered from normal to most deprioritized.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Priority {
    Normal,
    Low,
    Idle,
}

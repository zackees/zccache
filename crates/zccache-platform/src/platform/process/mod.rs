//! Neutral process-mechanics facade.

#[cfg(test)]
mod tests;

pub mod command;
pub mod exit;
pub mod inspect;
pub mod jobserver;
pub mod priority;
pub mod spawn;
pub mod stdio;
pub mod terminate;

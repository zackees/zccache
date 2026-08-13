//! Native exit and crash interpretation.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeExit { Success }

#[must_use]
pub fn crash_label(exit: NativeExit) -> &'static str {
    match exit { NativeExit::Success => "success" }
}

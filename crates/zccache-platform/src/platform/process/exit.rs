//! Native exit and crash interpretation.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeExit {
    Success,
}

#[must_use]
pub fn crash_label(exit: NativeExit) -> &'static str {
    match exit {
        NativeExit::Success => "success",
    }
}

#[must_use]
pub fn context_label(context: &crash_handler::CrashContext) -> String {
    crate::platform_imp::process::exit::context_label(context)
}

#[must_use]
pub fn context_summary(context: &crash_handler::CrashContext) -> String {
    crate::platform_imp::process::exit::context_summary(context)
}

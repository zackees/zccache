//! Native exit and crash interpretation.

/// Portable child-process termination metadata.
///
/// Normal exits retain their compiler-provided code. Unix signal exits use
/// `-(128 + signal)`, reserving `-1` for legacy unknown termination while
/// preserving the exact signal without widening established response types.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExitOutcome {
    pub exit_code: i32,
    pub termination_signal: Option<i32>,
}

#[must_use]
pub fn outcome(status: &std::process::ExitStatus) -> ExitOutcome {
    let termination_signal = crate::platform_imp::process::exit::termination_signal(status);
    let exit_code = termination_signal.map_or_else(
        || status.code().unwrap_or(-1),
        |signal| -(128_i32.saturating_add(signal)),
    );
    ExitOutcome {
        exit_code,
        termination_signal,
    }
}

/// Recover Unix signal metadata from the portable negative-signal response
/// encoding. Windows never treats a negative native exit code as a signal.
#[must_use]
pub fn termination_signal_from_exit_code(exit_code: i32) -> Option<i32> {
    crate::platform_imp::process::exit::termination_signal_from_exit_code(exit_code)
}

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

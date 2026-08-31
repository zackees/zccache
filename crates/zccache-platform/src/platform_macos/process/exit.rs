//! macOS exit interpretation.

use std::os::unix::process::ExitStatusExt as _;

pub fn termination_signal(status: &std::process::ExitStatus) -> Option<i32> {
    status.signal()
}

pub fn termination_signal_from_exit_code(exit_code: i32) -> Option<i32> {
    exit_code
        .checked_neg()
        .and_then(|encoded| encoded.checked_sub(128))
        .filter(|signal| (1..=127).contains(signal))
}

pub fn context_label(context: &crash_handler::CrashContext) -> String {
    match context.exception.as_ref() {
        Some(exception) => format!("EXC_{}", exception.kind),
        None => "SIGUNKNOWN".to_string(),
    }
}

pub fn context_summary(context: &crash_handler::CrashContext) -> String {
    match context.exception.as_ref() {
        Some(exception) => format!(
            "exception_kind = {}\nexception_code = {}\nexception_subcode = {:?}\nthread = {}",
            exception.kind, exception.code, exception.subcode, context.thread
        ),
        None => format!("exception = <none>\nthread = {}", context.thread),
    }
}

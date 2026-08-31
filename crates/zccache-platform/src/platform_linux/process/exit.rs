//! Linux exit interpretation.

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
    match context.siginfo.ssi_signo as i32 {
        libc::SIGSEGV => "SIGSEGV".to_string(),
        libc::SIGBUS => "SIGBUS".to_string(),
        libc::SIGILL => "SIGILL".to_string(),
        libc::SIGFPE => "SIGFPE".to_string(),
        libc::SIGABRT => "SIGABRT".to_string(),
        libc::SIGTRAP => "SIGTRAP".to_string(),
        other => format!("SIG{other}"),
    }
}

pub fn context_summary(context: &crash_handler::CrashContext) -> String {
    format!(
        "siginfo.si_signo = {}\nsiginfo.si_code  = {}\nsiginfo.si_addr  = {:#x}\ntid = {}",
        context.siginfo.ssi_signo,
        context.siginfo.ssi_code,
        context.siginfo.ssi_addr,
        context.tid
    )
}

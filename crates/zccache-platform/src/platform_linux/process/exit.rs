//! Linux exit interpretation.

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

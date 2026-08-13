//! macOS exit interpretation.

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

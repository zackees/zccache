//! Windows exit interpretation.

pub fn context_label(context: &crash_handler::CrashContext) -> String {
    let code = exception_code(context);
    match code {
        0xC0000005 => "STATUS_ACCESS_VIOLATION".to_string(),
        0xC000001D => "STATUS_ILLEGAL_INSTRUCTION".to_string(),
        0xC0000094 => "STATUS_INTEGER_DIVIDE_BY_ZERO".to_string(),
        0x80000003 => "STATUS_BREAKPOINT".to_string(),
        0xC00000FD => "STATUS_STACK_OVERFLOW".to_string(),
        code => format!("EXCEPTION_{code:08X}"),
    }
}

pub fn context_summary(context: &crash_handler::CrashContext) -> String {
    let (code, address) = exception_details(context);
    format!(
        "exception_code    = 0x{code:08X}\nexception_address = 0x{address:016X}\nthread_id         = {thread_id}",
        thread_id = context.thread_id
    )
}

fn exception_code(context: &crash_handler::CrashContext) -> u32 {
    exception_details(context).0
}

fn exception_details(context: &crash_handler::CrashContext) -> (u32, usize) {
    unsafe {
        if context.exception_pointers.is_null() {
            (0, 0)
        } else {
            let record = (*context.exception_pointers).ExceptionRecord;
            (
                (*record).ExceptionCode as u32,
                (*record).ExceptionAddress as usize,
            )
        }
    }
}

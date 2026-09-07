//! Windows exit interpretation.

pub fn termination_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

pub fn termination_signal_from_exit_code(_exit_code: i32) -> Option<i32> {
    None
}

//! macOS command setup.

pub fn hide_window(_command: &mut std::process::Command) {}

pub fn configure_process_group(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
        });
    }
}

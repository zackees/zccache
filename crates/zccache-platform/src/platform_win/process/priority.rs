//! Windows scheduling priority.

use std::os::windows::io::RawHandle;

use crate::process::priority::Priority;
use windows_sys::Win32::System::Threading::{
    SetPriorityClass, BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS,
};

pub fn apply_to_child(child: &tokio::process::Child, priority: Priority) -> std::io::Result<()> {
    let class = match priority {
        Priority::Normal => return Ok(()),
        Priority::Low => BELOW_NORMAL_PRIORITY_CLASS,
        Priority::Idle => IDLE_PRIORITY_CLASS,
        Priority::High => HIGH_PRIORITY_CLASS,
    };
    let Some(handle) = child.raw_handle() else {
        return Ok(());
    };
    apply_to_handle(handle, class)
}

fn apply_to_handle(handle: RawHandle, class: u32) -> std::io::Result<()> {
    if unsafe { SetPriorityClass(handle.cast(), class) } != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

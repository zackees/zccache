use std::process::{Child, Command};
use std::sync::OnceLock;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

pub fn sleeping_child(duration: Duration) -> std::io::Result<Child> {
    Command::new("powershell")
        .args(["-NoProfile", "-Command", &format!("Start-Sleep -Seconds {}", duration.as_secs().max(1))])
        .spawn()
}

pub fn attach_owner_death(child: &tokio::process::Child) -> std::io::Result<()> {
    let Some(handle) = child.raw_handle() else {
        return Ok(());
    };
    let job = DAEMON_JOB
        .get_or_init(|| WindowsJob::new().ok())
        .as_ref()
        .ok_or_else(|| std::io::Error::other("failed to create kill-on-close Job Object"))?;
    if unsafe { AssignProcessToJobObject(job.handle, handle.cast()) } != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

static DAEMON_JOB: OnceLock<Option<WindowsJob>> = OnceLock::new();

struct WindowsJob {
    handle: HANDLE,
}

impl WindowsJob {
    fn new() -> std::io::Result<Self> {
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&info).cast(),
                std::mem::size_of_val(&info) as u32,
            )
        };
        if configured == 0 {
            let error = std::io::Error::last_os_error();
            unsafe { CloseHandle(handle) };
            return Err(error);
        }
        Ok(Self { handle })
    }
}

impl Drop for WindowsJob {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

unsafe impl Send for WindowsJob {}
unsafe impl Sync for WindowsJob {}

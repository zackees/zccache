use std::io;
#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
    fn TerminateProcess(handle: isize, exit_code: u32) -> i32;
    fn CloseHandle(handle: isize) -> i32;
}
pub fn force(pid: u32) -> io::Result<()> {
    const PROCESS_TERMINATE: u32 = 0x0001;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    // SAFETY: handle is checked and closed exactly once.
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE, 0, pid);
        if handle == 0 { return Err(io::Error::last_os_error()); }
        let result = TerminateProcess(handle, 1);
        let error = (result == 0).then(io::Error::last_os_error);
        CloseHandle(handle);
        error.map_or(Ok(()), Err)
    }
}

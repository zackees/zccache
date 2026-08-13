use std::os::windows::ffi::OsStringExt;

#[repr(C)]
#[derive(Clone, Copy)]
struct FileTime { low: u32, high: u32 }

#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
    fn CloseHandle(handle: isize) -> i32;
    fn WaitForSingleObject(handle: isize, milliseconds: u32) -> u32;
    fn QueryFullProcessImageNameW(handle: isize, flags: u32, buffer: *mut u16, size: *mut u32) -> i32;
    fn GetProcessTimes(handle: isize, creation: *mut FileTime, exit: *mut FileTime, kernel: *mut FileTime, user: *mut FileTime) -> i32;
}
const QUERY: u32 = 0x1000;
const SYNCHRONIZE: u32 = 0x0010_0000;

pub fn is_alive(pid: u32) -> bool {
    // SAFETY: handle is checked and closed exactly once.
    unsafe {
        let handle = OpenProcess(QUERY | SYNCHRONIZE, 0, pid);
        if handle == 0 { return false; }
        let status = WaitForSingleObject(handle, 0);
        CloseHandle(handle);
        status == 0x102
    }
}
pub fn executable_path(pid: u32) -> Option<std::path::PathBuf> {
    // SAFETY: handle and writable UTF-16 buffer remain live for the call.
    unsafe {
        let handle = OpenProcess(QUERY, 0, pid);
        if handle == 0 { return None; }
        let mut buffer = vec![0u16; 32_768];
        let mut size = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size);
        CloseHandle(handle);
        (result != 0).then(|| std::path::PathBuf::from(std::ffi::OsString::from_wide(&buffer[..size as usize])))
    }
}
pub fn cpu_ticks(pid: u32) -> Option<u64> {
    // SAFETY: handle and all FILETIME output pointers remain live for the call.
    unsafe {
        let handle = OpenProcess(QUERY, 0, pid);
        if handle == 0 { return None; }
        let zero = FileTime { low: 0, high: 0 };
        let (mut creation, mut exit, mut kernel, mut user) = (zero, zero, zero, zero);
        let result = GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user);
        CloseHandle(handle);
        let value = |time: FileTime| ((time.high as u64) << 32) | u64::from(time.low);
        (result != 0).then(|| value(kernel).wrapping_add(value(user)))
    }
}

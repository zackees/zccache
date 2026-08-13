//! Windows standard-I/O mechanics.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, SetFilePointerEx, FILE_END, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_ALWAYS,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, SetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};

pub fn detach() {
    for (slot, access) in [
        (STD_INPUT_HANDLE, GENERIC_READ),
        (STD_OUTPUT_HANDLE, GENERIC_WRITE),
        (STD_ERROR_HANDLE, GENERIC_WRITE),
    ] {
        if let Some(handle) = open_file(OsStr::new("NUL"), access, OPEN_EXISTING) {
            replace_std_handle(slot, handle);
        }
    }
}

pub fn redirect_to_log(path: &Path) -> bool {
    if let Some(handle) = open_file(OsStr::new("NUL"), GENERIC_READ, OPEN_EXISTING) {
        replace_std_handle(STD_INPUT_HANDLE, handle);
    }
    let Some(log) = open_file(path.as_os_str(), GENERIC_WRITE, OPEN_ALWAYS) else {
        return false;
    };
    unsafe { SetFilePointerEx(log, 0, std::ptr::null_mut(), FILE_END) };
    replace_std_handle(STD_OUTPUT_HANDLE, log);
    replace_std_handle(STD_ERROR_HANDLE, log);
    true
}

fn open_file(path: &OsStr, access: u32, disposition: u32) -> Option<*mut std::ffi::c_void> {
    let path: Vec<u16> = path.encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            disposition,
            0,
            std::ptr::null_mut(),
        )
    };
    (!handle.is_null() && handle != INVALID_HANDLE_VALUE).then_some(handle)
}

fn replace_std_handle(slot: u32, handle: *mut std::ffi::c_void) {
    let old = unsafe { GetStdHandle(slot) };
    unsafe { SetStdHandle(slot, handle) };
    if !old.is_null() && old != INVALID_HANDLE_VALUE && old != handle {
        unsafe { CloseHandle(old) };
    }
}

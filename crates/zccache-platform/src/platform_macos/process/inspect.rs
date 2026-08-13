pub fn is_alive(pid: u32) -> bool {
    i32::try_from(pid).is_ok_and(|pid| unsafe { libc::kill(pid, 0) == 0 })
}
pub fn executable_path(pid: u32) -> Option<std::path::PathBuf> {
    const MAX: usize = 4096;
    let mut buffer = vec![0u8; MAX];
    let pid = i32::try_from(pid).ok()?;
    unsafe extern "C" { fn proc_pidpath(pid: i32, buffer: *mut std::ffi::c_void, size: u32) -> i32; }
    // SAFETY: buffer is writable for the supplied size and PID is scalar.
    let written = unsafe { proc_pidpath(pid, buffer.as_mut_ptr().cast(), MAX as u32) };
    if written <= 0 { return None; }
    buffer.truncate(written as usize);
    Some(std::path::PathBuf::from(std::str::from_utf8(&buffer).ok()?))
}
pub fn cpu_ticks(pid: u32) -> Option<u64> {
    let pid = i32::try_from(pid).ok()?;
    // SAFETY: zeroed POD filled by proc_pid_rusage for the matching flavor.
    let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, std::ptr::from_mut(&mut info).cast()) };
    (result == 0).then(|| info.ri_user_time.wrapping_add(info.ri_system_time))
}

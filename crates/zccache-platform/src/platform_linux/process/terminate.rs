pub fn force(pid: u32) -> std::io::Result<()> {
    let pid = i32::try_from(pid).map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "PID exceeds i32"))?;
    // SAFETY: fixed signal and scalar PID; no pointers are involved.
    if unsafe { libc::kill(pid, libc::SIGKILL) } == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

pub fn force_group(pid: u32) {
    unsafe { libc::kill(-(pid as i32), libc::SIGKILL); }
}

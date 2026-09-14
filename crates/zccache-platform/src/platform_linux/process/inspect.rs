pub fn is_alive(pid: u32) -> bool {
    i32::try_from(pid).is_ok_and(|pid| unsafe { libc::kill(pid, 0) == 0 })
}
pub fn executable_path(pid: u32) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}
pub fn cpu_ticks(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    Some(fields.get(11)?.parse::<u64>().ok()?.wrapping_add(fields.get(12)?.parse::<u64>().ok()?))
}
pub fn peak_rss_bytes(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let kib = status.lines().find_map(|line| line.strip_prefix("VmHWM:"))?.trim().strip_suffix("kB")?.trim();
    Some(kib.parse::<u64>().ok()?.saturating_mul(1024))
}

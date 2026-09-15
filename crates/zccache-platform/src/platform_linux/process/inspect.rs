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
pub const PEAK_RSS_READABLE_AFTER_EXIT: bool = false;
pub const MAX_TREE_PROCESSES: usize = 4096;
fn rss_bytes(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let kib = status.lines().find_map(|line| line.strip_prefix("VmRSS:"))?.trim().strip_suffix("kB")?.trim();
    Some(kib.parse::<u64>().ok()?.saturating_mul(1024))
}
pub fn tree_rss_bytes(pid: u32) -> Option<u64> {
    let mut total = rss_bytes(pid)?;
    let mut seen = std::collections::HashSet::from([pid]);
    let mut stack = vec![pid];
    while let Some(parent) = stack.pop() {
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{parent}/task")) else { continue };
        for task in tasks.flatten() {
            let Ok(children) = std::fs::read_to_string(task.path().join("children")) else { continue };
            for child in children.split_whitespace().filter_map(|value| value.parse::<u32>().ok()) {
                if seen.len() >= MAX_TREE_PROCESSES || !seen.insert(child) { continue; }
                if let Some(bytes) = rss_bytes(child) { total = total.saturating_add(bytes); }
                stack.push(child);
            }
        }
    }
    Some(total)
}

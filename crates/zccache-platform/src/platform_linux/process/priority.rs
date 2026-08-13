//! Linux scheduling priority.

use crate::process::priority::Priority;

pub fn apply_to_child(child: &tokio::process::Child, priority: Priority) -> std::io::Result<()> {
    let nice = match priority {
        Priority::Normal => return Ok(()),
        Priority::Low => 10,
        Priority::Idle => 19,
        Priority::High => -5,
    };
    let Some(pid) = child.id() else {
        return Ok(());
    };
    let pid = libc::id_t::try_from(pid)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "PID exceeds id_t"))?;
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, pid, nice) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

use std::process::{Child, Command};
use std::time::Duration;
pub fn sleeping_child(duration: Duration) -> std::io::Result<Child> {
    Command::new("powershell")
        .args(["-NoProfile", "-Command", &format!("Start-Sleep -Seconds {}", duration.as_secs().max(1))])
        .spawn()
}

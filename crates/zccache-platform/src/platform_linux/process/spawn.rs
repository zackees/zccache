use std::process::{Child, Command};
use std::time::Duration;

pub fn sleeping_child(duration: Duration) -> std::io::Result<Child> {
    Command::new("sleep").arg(duration.as_secs().max(1).to_string()).spawn()
}

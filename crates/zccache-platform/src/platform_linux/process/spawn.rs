use std::process::{Child, Command};
use std::time::Duration;

pub fn sleeping_child(duration: Duration) -> std::io::Result<Child> {
    Command::new("sleep").arg(duration.as_secs().max(1).to_string()).spawn()
}

pub fn echo_output(marker: &str) -> std::io::Result<std::process::Output> {
    Command::new("printf").arg("%s").arg(marker).output()
}

pub fn attach_owner_death(_child: &tokio::process::Child) -> std::io::Result<()> {
    Ok(())
}

pub fn uses_pre_spawn_owner_death() -> bool {
    true
}

pub fn run_cli_entry(entry: fn() -> std::process::ExitCode) -> std::process::ExitCode {
    entry()
}

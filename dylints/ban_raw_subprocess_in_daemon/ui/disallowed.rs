use std::process::Command;

fn main() {
    let mut command = Command::new("rustc");
    let _ = command.status();
}

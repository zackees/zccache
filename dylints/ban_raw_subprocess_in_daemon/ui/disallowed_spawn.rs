use std::process::Command;

fn main() {
    let mut command = Command::new("compiler");
    let _ = command.spawn();
}

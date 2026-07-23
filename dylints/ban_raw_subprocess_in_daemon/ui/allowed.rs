use std::process::Command;

fn main() {
    let mut command = Command::new("rustc");
    command.arg("--version");
    let _ = command.get_program();
}

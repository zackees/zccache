fn main() {
    let spawn = std::process::Command::spawn;
    let mut command = std::process::Command::new("rustc");
    let _ = spawn(&mut command);
}

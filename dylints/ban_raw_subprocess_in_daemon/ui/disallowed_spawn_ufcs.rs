fn main() {
    let mut command = std::process::Command::new("rustc");
    let _ = std::process::Command::spawn(&mut command);
}

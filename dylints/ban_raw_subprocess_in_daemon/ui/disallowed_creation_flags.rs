trait CommandExt {
    fn creation_flags(&mut self, flags: u32);
}

impl CommandExt for std::process::Command {
    fn creation_flags(&mut self, _flags: u32) {}
}

fn main() {
    let mut command = std::process::Command::new("compiler");
    command.creation_flags(0x0800_0000);
}

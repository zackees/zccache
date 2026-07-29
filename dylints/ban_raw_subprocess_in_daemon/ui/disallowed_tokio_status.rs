mod tokio {
    pub mod process {
        pub struct Command;

        impl Command {
            pub fn status(&mut self) {}
        }
    }
}

fn main() {
    let mut command = tokio::process::Command;
    command.status();
}

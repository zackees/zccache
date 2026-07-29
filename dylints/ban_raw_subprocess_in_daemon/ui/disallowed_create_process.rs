extern "C" {
    fn CreateProcessW();
}

fn main() {
    unsafe {
        CreateProcessW();
    }
}

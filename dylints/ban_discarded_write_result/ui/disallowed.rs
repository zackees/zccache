use std::sync::mpsc;

fn main() {
    let _ = std::fs::write("a", b"b");

    let (tx, _rx) = mpsc::channel::<u8>();
    let _ = tx.send(1);

    std::fs::rename("a", "b").ok();
}

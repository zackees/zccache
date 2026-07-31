fn gated() -> std::io::Result<()> {
    // The gated-on-success form is the fix; it must not fire.
    if let Err(error) = std::fs::write("a", b"b") {
        return Err(error);
    }
    Ok(())
}

fn main() {
    let _ = gated();

    // Discarding a non-`Result` is fine.
    let _ = println!("hello");

    // Discarding a non-write `Result` is out of the matched name set.
    let s = "7";
    let _ = s.parse::<u8>();

    // Cleanup-shaped names are deliberately not matched.
    let _ = std::fs::remove_file("a");
}

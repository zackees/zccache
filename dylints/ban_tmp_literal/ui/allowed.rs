fn main() {
    // Neutral fake paths for fs-inert fixtures do not trigger the lint.
    let _key = "/fixture/persist0.c";
    // Paths that merely contain "tmp" elsewhere are fine.
    let _other = "/var/tmp/x";
    let _name = "tmp/relative";
    // Building on the sanctioned scratch root is the runtime replacement.
    let _joined = std::path::Path::new("/some/configured/dir").join("scratch");
}

fn main() {
    let _td = tempfile::tempdir().unwrap();
    let _td2 = tempfile::TempDir::new().unwrap();
    let _nf = tempfile::NamedTempFile::new().unwrap();
    let _t = std::env::temp_dir();
}

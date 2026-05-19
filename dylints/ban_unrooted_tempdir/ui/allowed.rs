use std::path::Path;

fn main() {
    let base = Path::new("/some/configured/dir");
    let _td = tempfile::tempdir_in(base).unwrap();
    let _td2 = tempfile::TempDir::new_in(base).unwrap();
    let _nf = tempfile::NamedTempFile::new_in(base).unwrap();
}

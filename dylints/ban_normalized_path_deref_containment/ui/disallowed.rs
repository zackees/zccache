use std::ops::Deref;
use std::path::{Path, PathBuf};

struct NormalizedPath(PathBuf);

impl Deref for NormalizedPath {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

fn main() {
    let path = NormalizedPath(PathBuf::from("sdk/include/vector"));
    let _ = path.starts_with("sdk/include");
    let _ = path.strip_prefix("sdk/include");
}

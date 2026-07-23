use std::path::{Path, PathBuf};

struct NormalizedPath(PathBuf);

impl NormalizedPath {
    fn starts_with(&self, base: impl AsRef<Path>) -> bool {
        self.0.starts_with(base)
    }

    fn strip_prefix(&self, base: impl AsRef<Path>) -> Option<PathBuf> {
        self.0.strip_prefix(base).ok().map(Path::to_path_buf)
    }
}

fn main() {
    let path = NormalizedPath(PathBuf::from("sdk/include/vector"));
    let _ = path.starts_with("sdk/include");
    let _ = path.strip_prefix("sdk/include");
}

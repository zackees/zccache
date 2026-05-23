use std::fs;
use std::path::Path;

use zccache_fingerprint::ScannedFile;

/// Create a file with content, creating parent dirs as needed.
pub fn create_file(dir: &Path, rel: &str, content: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, content).unwrap();
}

/// Extract relative paths as a sorted Vec for assertion.
#[allow(dead_code)]
pub fn rel_paths(files: &[ScannedFile]) -> Vec<&str> {
    files.iter().map(|f| f.relative.as_str()).collect()
}

/// Wait for filesystem mtime to change.
/// On Windows NTFS, mtime granularity may not advance on rapid writes.
#[allow(dead_code)]
pub fn wait_for_mtime_change() {
    std::thread::sleep(std::time::Duration::from_millis(1100));
}

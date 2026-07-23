use std::path::{Path, PathBuf};

fn resolve_artifact_payloads(
    artifact_dir: &Path,
    key_hex: &str,
    output_count: usize,
) -> Vec<PathBuf> {
    let _ = (artifact_dir, key_hex, output_count);
    Vec::new()
}

fn main() {
    let _ = resolve_artifact_payloads(Path::new("artifacts"), "0123abcd", 1);
}

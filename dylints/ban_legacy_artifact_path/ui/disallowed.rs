use std::path::Path;

fn direct(artifact_dir: &Path, key_hex: &str, i: usize) {
    let _ = artifact_dir.join(format!("{key_hex}_{i}"));
}

fn split(artifact_dir: &Path, artifact_key: &str, index: usize) {
    let legacy_name = format!("{artifact_key}_{index}");
    let _ = artifact_dir.join(legacy_name);
}

fn concatenated(artifact_dir: &Path, key_hex: &str, index: usize) {
    let legacy_name = key_hex.to_string() + "_" + &index.to_string();
    let _ = artifact_dir.join(legacy_name);
}

fn main() {}

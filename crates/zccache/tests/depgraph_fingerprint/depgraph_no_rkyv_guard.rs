//! Guard for #1661: the depgraph snapshot is bincode 1 via serde, and
//! `rkyv` is gone from the workspace. Re-adding it (directly or as a
//! transitive dependency) fails here.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn cargo_lock_has_no_rkyv_package() {
    let lock_path = workspace_root().join("Cargo.lock");
    let lock = std::fs::read_to_string(&lock_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", lock_path.display()));
    let offenders: Vec<&str> = lock
        .lines()
        .map(str::trim)
        .filter(|l| *l == "name = \"rkyv\"" || l.starts_with("name = \"rkyv_"))
        .collect();
    assert!(
        offenders.is_empty(),
        "Cargo.lock still contains rkyv packages {offenders:?}; the depgraph \
         snapshot uses bincode 1 (#1661)"
    );
}

fn collect_manifests(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            if name == "target" || name == ".git" {
                continue;
            }
            collect_manifests(&path, out);
        } else if name == "Cargo.toml" {
            out.push(path);
        }
    }
}

#[test]
fn no_manifest_mentions_rkyv() {
    let root = workspace_root();
    let mut manifests = vec![root.join("Cargo.toml")];
    collect_manifests(&root.join("crates"), &mut manifests);
    assert!(
        manifests.len() > 1,
        "found no crate manifests under crates/"
    );
    let offenders: Vec<String> = manifests
        .iter()
        .filter(|p| {
            std::fs::read_to_string(p)
                .map(|s| s.contains("rkyv"))
                .unwrap_or(false)
        })
        .map(|p| p.display().to_string())
        .collect();
    assert!(
        offenders.is_empty(),
        "manifests still mention rkyv: {offenders:?} (#1661)"
    );
}

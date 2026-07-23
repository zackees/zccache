/// Restores the caller's working directory after a UI test.
pub(crate) struct CurrentDirGuard(std::path::PathBuf);

impl CurrentDirGuard {
    fn set(path: &std::path::Path) -> Self {
        let previous = std::env::current_dir().expect("current dir should be readable");
        std::env::set_current_dir(path).expect("current dir should switch to manifest dir");
        Self(previous)
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.0).expect("current dir should be restored");
    }
}

fn prepare_dylint_library(manifest_dir: &std::path::Path, crate_name: &str) {
    let toolchain = std::env::var("RUSTUP_TOOLCHAIN").expect("RUSTUP_TOOLCHAIN should be set");
    let library_name = crate_name.replace('-', "_");
    let target_root = std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("target"));
    let target_debug = target_root.join("debug");
    let expected = target_debug.join(format!(
        "{}{}@{}{}",
        std::env::consts::DLL_PREFIX,
        library_name,
        toolchain,
        std::env::consts::DLL_SUFFIX
    ));
    let plain = target_debug.join(format!(
        "{}{}{}",
        std::env::consts::DLL_PREFIX,
        library_name,
        std::env::consts::DLL_SUFFIX
    ));
    if plain.exists() {
        std::fs::copy(&plain, &expected)
            .expect("toolchain-suffixed dylint library should be copied");
        return;
    }

    let deps_dir = target_debug.join("deps");
    for entry in std::fs::read_dir(&deps_dir).expect("deps dir should be readable") {
        let path = entry.expect("deps entry should be readable").path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if name.starts_with(&format!("{}{}", std::env::consts::DLL_PREFIX, library_name))
            && name.ends_with(std::env::consts::DLL_SUFFIX)
        {
            std::fs::copy(&path, &expected)
                .expect("hashed dylint library should be copied to the expected filename");
            return;
        }
    }

    if expected.exists() {
        return;
    }

    panic!(
        "could not find a built dylint library to copy into {}",
        expected.display()
    );
}

pub(crate) fn run(crate_name: &str, manifest_dir: &str) {
    let manifest_dir = std::path::Path::new(manifest_dir);
    let _guard = CurrentDirGuard::set(manifest_dir);
    prepare_dylint_library(manifest_dir, crate_name);
    dylint_testing::ui_test(crate_name, "ui");
}

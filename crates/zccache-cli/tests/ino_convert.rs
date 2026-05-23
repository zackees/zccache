use std::fs;
use std::thread;
use std::time::Duration;

fn libclang_available() -> bool {
    zccache_monocrate::compiler::arduino::can_load_libclang()
}

#[test]
fn cached_ino_conversion_skips_rewriting_unchanged_output() {
    if !libclang_available() {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let ino = dir.path().join("CacheMe.ino");
    let out = dir.path().join("CacheMe.ino.cpp");
    fs::write(
        &ino,
        r#"
void setup() {}
void loop() {}

int helper(int value) {
    return value + 1;
}
"#,
    )
    .unwrap();

    let first =
        zccache_cli::run_ino_convert_cached(&ino, &out, &zccache_cli::InoConvertOptions::default())
            .unwrap();
    assert!(out.exists());
    assert!(
        !first.cache_hit,
        "first conversion should populate the cache"
    );

    let first_mtime = fs::metadata(&out).unwrap().modified().unwrap();
    thread::sleep(Duration::from_millis(1100));

    let second =
        zccache_cli::run_ino_convert_cached(&ino, &out, &zccache_cli::InoConvertOptions::default())
            .unwrap();
    let second_mtime = fs::metadata(&out).unwrap().modified().unwrap();

    assert!(second.cache_hit, "second conversion should come from cache");
    assert!(
        second.skipped_write,
        "second conversion should preserve the existing output file when unchanged"
    );
    assert_eq!(
        first_mtime, second_mtime,
        "unchanged cached output should not be rewritten"
    );
}

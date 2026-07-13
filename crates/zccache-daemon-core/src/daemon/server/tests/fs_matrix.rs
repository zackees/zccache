//! Parameterized real-filesystem acceptance matrix for #1039.

use super::super::*;
use std::io::{Seek, SeekFrom, Write};
use zccache_test_support::{FixtureResult, FsFixture};

type Builder = fn() -> FixtureResult;
type MatrixRow = (&'static str, bool, Builder);

#[test]
fn filesystem_materialization_matrix_prints_loud_summary() {
    let builders = platform_builders();
    let total = builders.len();
    let mut executed = Vec::new();
    let mut executed_names = Vec::new();
    let mut skipped = Vec::new();
    for (row_name, cross_volume, builder) in builders {
        match builder() {
            Ok(fixture) => {
                let evidence = exercise_row(&fixture, cross_volume);
                executed_names.push(row_name);
                executed.push(format!("{row_name} ({evidence})"));
            }
            Err(skip) => skipped.push(format!("{} ({})", skip.name, skip.reason)),
        }
    }
    println!(
        "filesystem matrix summary: executed: {}/{} rows: {}; skipped: {}",
        executed.len(),
        total,
        executed.join(", "),
        if skipped.is_empty() {
            "none".to_string()
        } else {
            skipped.join(", ")
        }
    );
    assert!(!executed.is_empty(), "at least the native row must execute");
    if std::env::var_os("ZCCACHE_REQUIRE_FS_MATRIX").is_some() {
        assert_required_ci_rows(&executed_names);
    }
}

fn assert_required_ci_rows(executed: &[&str]) {
    #[cfg(windows)]
    let required = [
        "windows-ntfs-native",
        "refs-vhdx",
        "fat32-vhdx",
        "exfat-vhdx",
        "smb-loopback",
        "windows-cross-volume",
    ];
    #[cfg(target_os = "linux")]
    let required = [
        "linux-native-overlayfs",
        "ext4-loop",
        "btrfs-loop",
        "tmpfs",
        "linux-cross-volume-tmpfs",
        "vfat-loop",
    ];
    #[cfg(target_os = "macos")]
    let required = [
        "apfs-native",
        "hfs-image",
        "mac-cross-volume-hfs",
        "exfat-image",
    ];
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    let required: [&str; 0] = [];

    for name in required {
        assert!(
            executed.contains(&name),
            "required CI filesystem row {name} was skipped; see the loud matrix summary above"
        );
    }
}

fn platform_builders() -> Vec<MatrixRow> {
    #[cfg(windows)]
    return vec![
        ("windows-ntfs-native", false, || {
            FsFixture::native("windows-ntfs-native")
        }),
        ("refs-vhdx", false, FsFixture::refs_vhdx),
        ("fat32-vhdx", false, FsFixture::fat32_vhdx),
        ("exfat-vhdx", false, FsFixture::exfat_vhdx),
        ("smb-loopback", false, FsFixture::smb_loopback),
        ("windows-cross-volume", true, FsFixture::second_volume_vhdx),
    ];
    #[cfg(target_os = "linux")]
    return vec![
        ("linux-native-overlayfs", false, || {
            FsFixture::native("linux-native-overlayfs")
        }),
        ("ext4-loop", false, FsFixture::ext4_loop),
        ("btrfs-loop", false, FsFixture::btrfs_loop),
        ("tmpfs", false, FsFixture::tmpfs),
        ("linux-cross-volume-tmpfs", true, FsFixture::tmpfs),
        ("vfat-loop", false, FsFixture::vfat_loop),
        ("nfs", false, FsFixture::nfs),
    ];
    #[cfg(target_os = "macos")]
    return vec![
        ("apfs-native", false, FsFixture::apfs_native),
        ("hfs-image", false, FsFixture::hfs_image),
        ("mac-cross-volume-hfs", true, FsFixture::hfs_image),
        ("exfat-image", false, FsFixture::exfat_image),
    ];
    #[allow(unreachable_code)]
    Vec::new()
}

fn exercise_row(fixture: &FsFixture, cross_volume: bool) -> String {
    let source_fixture = cross_volume.then(|| tempfile::tempdir().unwrap());
    let source_root = source_fixture
        .as_ref()
        .map_or_else(|| fixture.root(), |temp| temp.path());
    let blob = source_root.join("blob.rlib");
    let output = fixture.root().join("output.rlib");
    let original = b"matrix-original-bytes";
    std::fs::write(&blob, original).unwrap();
    write_authoritative_blob_digest(&blob).unwrap();
    let old_time = filetime::FileTime::from_unix_time(1_000_000_000, 123);
    filetime::set_file_mtime(&blob, old_time).unwrap();
    // FAT/exFAT store mtime with 2-second granularity, so the value that
    // actually lands on disk can differ from what was requested. Compare
    // the materialized mtime against the blob's *actual* stored mtime
    // (which `restore_cache_mtime` reads and propagates) rather than the
    // pre-rounding `old_time` we asked for.
    let blob_time =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&blob).unwrap());
    let caps = fs_caps(&blob, &output);
    if cross_volume {
        assert!(!caps.reflink && !caps.hardlink);
    }
    let observed = write_cached_file_observed(&output, &blob).unwrap();
    let output_time =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&output).unwrap());
    assert_eq!(output_time.unix_seconds(), blob_time.unix_seconds());
    let shares_file_identity = same_file(&blob, &output);

    let tier = if observed.reflink_count == 1 {
        assert!(!shares_file_identity);
        make_writable(&output).unwrap();
        std::fs::write(&output, b"private").unwrap();
        assert_eq!(std::fs::read(&blob).unwrap(), original);
        "reflink"
    } else if observed.hardlink_count == 1 {
        assert!(shares_file_identity);
        let mutation = std::fs::write(&output, b"poison");
        if mutation.is_ok() {
            mark_registered_links_suspect([output.as_path()]);
            assert!(
                write_cached_output(&fixture.root().join("second.rlib"), &blob, original).is_err()
            );
        } else {
            assert_eq!(std::fs::read(&blob).unwrap(), original);
        }
        "hardlink-cow-lite"
    } else if observed.copy_count == 1 {
        assert!(!shares_file_identity);
        std::fs::write(&output, b"private").unwrap();
        assert_eq!(std::fs::read(&blob).unwrap(), original);
        "copy"
    } else {
        panic!("materialization must report exactly one physical tier")
    };
    let copied_bytes = observed.copy_bytes;
    let mutation_behavior = if tier == "hardlink-cow-lite" {
        "detected-or-blocked"
    } else {
        "isolated"
    };
    let file_identity = if shares_file_identity {
        "shared-protected"
    } else {
        "independent"
    };
    let _ = make_writable(&blob);
    let _ = make_writable(&output);
    if output.exists() {
        std::fs::remove_file(&output).unwrap();
    }
    std::fs::remove_file(&blob).unwrap();
    let cleanup = !output.exists() && !blob.exists();
    assert!(cleanup);
    format!(
        "tier={tier} copied_bytes={copied_bytes} mutation_behavior={mutation_behavior} mtime_preserved=true file_identity={file_identity} cleanup={cleanup}"
    )
}

/// ReFS uses cluster-rounded duplicate-extents calls for non-aligned lengths.
#[cfg(windows)]
#[test]
fn refs_non_cluster_multiple_round_trips() {
    let fixture = match FsFixture::refs_vhdx() {
        Ok(fixture) => fixture,
        Err(skip) if std::env::var_os("ZCCACHE_REQUIRE_REFS").is_none() => {
            println!("ReFS acceptance: skipped ({})", skip.reason);
            return;
        }
        Err(skip) => panic!("required ReFS fixture unavailable: {skip:?}"),
    };
    let blob = fixture.root().join("unaligned.bin");
    let output = fixture.root().join("unaligned-out.bin");
    let bytes = vec![0xa5; 64 * 1024 + 17];
    std::fs::write(&blob, &bytes).unwrap();
    let old_time = filetime::FileTime::from_unix_time(1_000_000_000, 100);
    filetime::set_file_mtime(&blob, old_time).unwrap();
    write_authoritative_blob_digest(&blob).unwrap();
    let observed = write_cached_file_observed(&output, &blob).unwrap();
    assert_eq!(observed.reflink_count, 1, "ReFS must use the reflink tier");
    assert_eq!(observed.copy_bytes, 0);
    assert!(!same_file(&blob, &output));
    assert_eq!(std::fs::read(&output).unwrap(), bytes);
    let output_time =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&output).unwrap());
    assert_eq!(output_time.unix_seconds(), old_time.unix_seconds());
    std::fs::write(&output, b"private").unwrap();
    assert_eq!(std::fs::read(&blob).unwrap(), bytes);
    std::fs::remove_file(&output).unwrap();
    remove_registered_blob(&blob).unwrap();
    assert!(!output.exists() && !blob.exists());
    println!(
        "ReFS acceptance: executed tier=reflink copied_bytes=0 mutation_isolated=true mtime_preserved=true file_identity=independent cleanup=true"
    );
}

/// >4 GiB clone chunking executes only in the bounded scheduled/manual gate.
#[test]
fn reflink_larger_than_four_gib_uses_chunked_clone() {
    if std::env::var_os("ZCCACHE_REQUIRE_LARGE_COW").is_none() {
        println!("large COW acceptance: skipped (ZCCACHE_REQUIRE_LARGE_COW is unset)");
        return;
    }
    let fixture = platform_reflink_fixture().expect("reflink fixture prerequisite unavailable");
    let blob = fixture.root().join("large.bin");
    let output = fixture.root().join("large-out.bin");
    const FOUR_GIB: u64 = 4 * 1024 * 1024 * 1024;
    const LENGTH: u64 = FOUR_GIB + 64 * 1024 + 17;
    let mut file = std::fs::File::create(&blob).unwrap();
    file.set_len(LENGTH).unwrap();
    for (offset, marker) in [
        (0, b"head".as_slice()),
        (FOUR_GIB - 7, b"boundary-before".as_slice()),
        (FOUR_GIB + 9, b"boundary-after".as_slice()),
        (LENGTH - 4, b"tail".as_slice()),
    ] {
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(marker).unwrap();
    }
    file.sync_all().unwrap();
    let old_time = filetime::FileTime::from_unix_time(1_000_000_000, 100);
    filetime::set_file_mtime(&blob, old_time).unwrap();
    drop(file);
    register_trusted_blob_for_test(&blob).unwrap();
    let caps = fs_caps(&blob, &output);
    assert!(caps.reflink, "stress fixture must support reflinks");
    let observed = write_cached_file_observed(&output, &blob).unwrap();
    assert_eq!(observed.reflink_count, 1);
    assert_eq!(observed.copy_bytes, 0);
    assert_eq!(std::fs::metadata(&output).unwrap().len(), LENGTH);
    assert!(!same_file(&blob, &output));
    assert_eq!(read_at(&output, FOUR_GIB - 7, 15), b"boundary-before");
    assert_eq!(read_at(&output, FOUR_GIB + 9, 14), b"boundary-after");
    let output_time =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&output).unwrap());
    assert_eq!(output_time.unix_seconds(), old_time.unix_seconds());
    let mut output_file = std::fs::OpenOptions::new()
        .write(true)
        .open(&output)
        .unwrap();
    output_file.seek(SeekFrom::Start(FOUR_GIB + 9)).unwrap();
    output_file.write_all(b"private-output").unwrap();
    output_file.sync_all().unwrap();
    assert_eq!(read_at(&blob, FOUR_GIB + 9, 14), b"boundary-after");
    std::fs::remove_file(&output).unwrap();
    remove_registered_blob(&blob).unwrap();
    assert!(!output.exists() && !blob.exists());
    println!(
        "large COW acceptance: executed fixture={} bytes={LENGTH} tier=reflink copied_bytes=0 mutation_isolated=true mtime_preserved=true file_identity=independent cleanup=true",
        fixture.name()
    );
}

fn read_at(path: &std::path::Path, offset: u64, length: usize) -> Vec<u8> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    let mut bytes = vec![0; length];
    file.read_exact(&mut bytes).unwrap();
    bytes
}

fn platform_reflink_fixture() -> FixtureResult {
    #[cfg(windows)]
    return FsFixture::refs_vhdx();
    #[cfg(target_os = "linux")]
    return FsFixture::btrfs_loop();
    #[cfg(target_os = "macos")]
    return FsFixture::apfs_native();
    #[allow(unreachable_code)]
    FsFixture::native("unsupported")
}

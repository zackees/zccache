use super::*;

#[test]
fn mode_parses_each_canonical_value() {
    for mode in MaterializationMode::ALL {
        assert_eq!(MaterializationMode::parse(mode.as_str()), Ok(Some(mode)));
    }
}

#[test]
fn mode_parse_is_case_and_whitespace_insensitive() {
    let cases = [
        (" reflink\n", MaterializationMode::Reflink),
        ("Copy", MaterializationMode::Copy),
        ("link", MaterializationMode::Link),
        ("\tAuTo ", MaterializationMode::Auto),
    ];
    for (raw, expected) in cases {
        assert_eq!(
            MaterializationMode::parse(raw),
            Ok(Some(expected)),
            "{raw:?}"
        );
    }
}

#[test]
fn mode_unset_or_empty_is_absent_not_auto() {
    assert_eq!(parse_materialization_mode(None), Ok(None));
    assert_eq!(parse_materialization_mode(Some("")), Ok(None));
    assert_eq!(parse_materialization_mode(Some("   ")), Ok(None));
}

#[test]
fn mode_rejects_unknown_values_listing_valid_ones() {
    for raw in ["hardlink", "cow", "1", "REFLINK_ALWAYS", "auto-ish"] {
        let error = MaterializationMode::parse(raw).unwrap_err();
        assert_eq!(error.value(), raw);
        let message = error.to_string();
        assert!(message.contains(MATERIALIZATION_MODE_ENV), "{message}");
        for mode in MaterializationMode::ALL {
            assert!(message.contains(mode.as_str()), "{message} lacks {mode}");
        }
    }
}

#[test]
fn mode_display_round_trips_and_from_str_requires_a_value() {
    for mode in MaterializationMode::ALL {
        assert_eq!(mode.to_string().parse::<MaterializationMode>(), Ok(mode));
    }
    assert!("".parse::<MaterializationMode>().is_err());
}

#[test]
fn mode_default_is_auto() {
    assert_eq!(MaterializationMode::default(), MaterializationMode::Auto);
}

#[test]
fn client_env_lookup_ignores_other_variables_and_empty_values() {
    let env = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect::<Vec<_>>()
    };
    assert_eq!(materialization_mode_from_client_env(None), Ok(None));
    assert_eq!(
        materialization_mode_from_client_env(Some(&env(&[("ZCCACHE_MODEX", "COPY")]))),
        Ok(None)
    );
    assert_eq!(
        materialization_mode_from_client_env(Some(&env(&[(MATERIALIZATION_MODE_ENV, "")]))),
        Ok(None)
    );
    assert_eq!(
        materialization_mode_from_client_env(Some(&env(&[
            ("PATH", "/bin"),
            (MATERIALIZATION_MODE_ENV, "reflink"),
        ]))),
        Ok(Some(MaterializationMode::Reflink))
    );
    assert!(materialization_mode_from_client_env(Some(&env(&[(
        MATERIALIZATION_MODE_ENV,
        "bogus"
    )])))
    .is_err());
}

/// This module is the single owner of the `ZCCACHE_MODE` name: every other
/// crate goes through its typed accessors, so one grammar applies everywhere.
/// Documentation and tests may mention the name; production code may not
/// spell it.
#[test]
fn no_raw_zccache_mode_reads_outside_owner() {
    let needle = concat!("\"ZCCACHE_", "MODE\"");
    let offenders: Vec<String> = production_sources()
        .into_iter()
        .filter(|(relative, source)| {
            source.contains(needle) && relative != "zccache-core/src/config/materialization_mode.rs"
        })
        .map(|(relative, _)| relative)
        .collect();
    assert!(
        offenders.is_empty(),
        "read ZCCACHE_MODE through zccache_core::config's accessors, not by name: {offenders:?}"
    );
}

#[test]
fn shareable_tiers_per_mode() {
    let tiers = |mode: MaterializationMode| {
        let tiers = mode.tiers_for_shareable();
        (tiers.reflink, tiers.hardlink)
    };
    assert_eq!(tiers(MaterializationMode::Auto), (true, true));
    assert_eq!(tiers(MaterializationMode::Link), (false, true));
    assert_eq!(tiers(MaterializationMode::Copy), (false, false));
    assert_eq!(tiers(MaterializationMode::Reflink), (true, false));
}

/// COPY must own its blocks even where `std::fs::copy` would clone
/// (`copy_file_range` on btrfs/XFS); other modes keep the fast path. On a
/// volume that cannot share blocks both results are trivially exclusive.
#[test]
fn copy_mode_copy_file_owns_its_blocks() {
    use kernal_api::platform::fs::{extent_sharing, ExtentSharing};
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.bin");
    let bytes: Vec<u8> = (0..512 * 1024_u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&source, &bytes).unwrap();

    let copied = dir.path().join("copied.bin");
    let written = MaterializationMode::Copy
        .copy_file(&source, &copied)
        .unwrap();
    assert_eq!(written, bytes.len() as u64);
    assert_eq!(std::fs::read(&copied).unwrap(), bytes);
    assert!(
        !matches!(extent_sharing(&copied), Ok(ExtentSharing::Shared)),
        "COPY must not share blocks with its source"
    );
    assert_eq!(
        std::fs::metadata(&copied).unwrap().permissions(),
        std::fs::metadata(&source).unwrap().permissions()
    );

    let fast = dir.path().join("fast.bin");
    MaterializationMode::Auto.copy_file(&source, &fast).unwrap();
    assert_eq!(std::fs::read(&fast).unwrap(), bytes);
}

/// COPY creates its destination exclusively, so it can never truncate a
/// file (for example one a racing delivery just hardlinked to the cache
/// blob), and a failed copy leaves no partial destination behind.
#[test]
fn copy_mode_copy_file_refuses_existing_destinations_and_cleans_up_failures() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.bin");
    std::fs::write(&source, b"fresh bytes").unwrap();
    let existing = dir.path().join("existing.bin");
    std::fs::write(&existing, b"someone else's bytes").unwrap();
    let error = MaterializationMode::Copy
        .copy_file(&source, &existing)
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&existing).unwrap(), b"someone else's bytes");

    // Reading a directory fails after the destination was created.
    let failed = dir.path().join("failed.bin");
    assert!(MaterializationMode::Copy
        .copy_file(dir.path(), &failed)
        .is_err());
    assert!(
        !failed.exists(),
        "a failed copy must not leave a partial file"
    );
}

/// Every raw hardlink or reflink call sits in a module that plans its tiers
/// from `ZCCACHE_MODE` (#1683). A new call site must route through one of
/// them, or join this list with the mode applied.
#[test]
fn raw_link_and_clone_calls_stay_in_mode_aware_modules() {
    const ALLOWED: &[&str] = &[
        // Cache-hit executor and its capability probe.
        "zccache-daemon-core/src/daemon/server/persist/write_cached.rs",
        "zccache-daemon-core/src/daemon/server/persist/fs_caps.rs",
        // Store direction (`plan_store_tiers`).
        "zccache-daemon-core/src/daemon/server/persist/artifact_io.rs",
        // Independent staged delivery (`copy_output_with`).
        "zccache-daemon-core/src/daemon/server/persist/staged_store.rs",
        // `zccache warm` and rust-plan bundles (`tiers_for_shareable`).
        "zccache-cli-core/src/cli/commands/warm_delivery.rs",
        "zccache-artifact/src/rust_plan/local.rs",
    ];
    let needles = [concat!("fs::hard_", "link("), concat!("reflink_", "file(")];
    let offenders: Vec<String> = production_sources()
        .into_iter()
        .filter(|(relative, source)| {
            needles.iter().any(|needle| source.contains(needle))
                && !ALLOWED.contains(&relative.as_str())
        })
        .map(|(relative, _)| relative)
        .collect();
    assert!(
        offenders.is_empty(),
        "raw hardlink/reflink outside ZCCACHE_MODE-aware modules: {offenders:?}"
    );
}

/// `(path relative to crates/, contents)` of every production `.rs` file:
/// test modules, test-support crates, benches and build output excluded.
fn production_sources() -> Vec<(String, String)> {
    let crates_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir");
    let mut sources = Vec::new();
    let mut stack = vec![crates_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                let skip = matches!(
                    name.as_ref(),
                    "target" | "tests" | "benches" | "test_support" | "zccache-test-support"
                ) || name.starts_with('.');
                if !skip {
                    stack.push(path);
                }
                continue;
            }
            if !name.ends_with(".rs") || name.contains("test") {
                continue;
            }
            let relative = path
                .strip_prefix(crates_dir)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            sources.push((relative, std::fs::read_to_string(&path).unwrap()));
        }
    }
    sources
}

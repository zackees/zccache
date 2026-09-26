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
    let crates_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir");
    let needle = concat!("\"ZCCACHE_", "MODE\"");
    let mut offenders = Vec::new();
    let mut stack = vec![crates_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                if name != "target" && name != "tests" && !name.starts_with('.') {
                    stack.push(path);
                }
                continue;
            }
            if !name.ends_with(".rs") || name.contains("test") {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            if source.contains(needle) && !path.ends_with("config/materialization_mode.rs") {
                offenders.push(path.display().to_string());
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "read ZCCACHE_MODE through zccache_core::config's accessors, not by name: {offenders:?}"
    );
}

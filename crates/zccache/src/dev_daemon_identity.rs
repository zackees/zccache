//! Development-build daemon namespace initialization (issue #1362).

use std::ffi::OsString;
use std::io;

const HASH_PREFIX_BYTES: usize = 8;

fn namespace_for_process<F>(
    inherited: Option<OsString>,
    release_build: bool,
    hash_current_exe: F,
) -> io::Result<Option<String>>
where
    F: FnOnce() -> io::Result<[u8; 32]>,
{
    if inherited
        .as_deref()
        .and_then(|value| {
            crate::core::config::namespace::sanitize_daemon_namespace(&value.to_string_lossy())
        })
        .is_some()
    {
        return Ok(None);
    }
    if release_build {
        return Ok(None);
    }

    let hash = blake3::Hash::from_bytes(hash_current_exe()?);
    let hex = hash.to_hex();
    let hash_prefix = &hex.as_str()[..HASH_PREFIX_BYTES * 2];
    Ok(Some(format!("{}-{hash_prefix}", crate::core::VERSION)))
}

/// Establish the daemon namespace before CLI or daemon configuration is read.
pub fn initialize() -> io::Result<()> {
    let inherited = std::env::var_os(crate::core::config::DAEMON_NAMESPACE_ENV);
    if inherited
        .as_deref()
        .and_then(|value| {
            crate::core::config::namespace::sanitize_daemon_namespace(&value.to_string_lossy())
        })
        .is_some()
    {
        return Ok(());
    }

    let current_exe = crate::platform::executable::current_image().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot locate the development zccache executable: {error}"),
        )
    })?;
    let release_build = crate::symbols::read_marker_from_path(&current_exe).is_some();
    let namespace = namespace_for_process(inherited, release_build, || {
        running_process::blake3_file(&current_exe)
            .map(|hash| *hash.as_bytes())
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "cannot hash development zccache executable {}: {error}",
                        current_exe.display()
                    ),
                )
            })
    })?;
    if let Some(namespace) = namespace {
        // The binary entrypoints call this before CLI/daemon initialization.
        // The value then flows to compiler children and the spawned daemon.
        std::env::set_var(crate::core::config::DAEMON_NAMESPACE_ENV, namespace);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn hash(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    #[test]
    fn inherited_namespace_wins_without_hashing() {
        let hashed = Cell::new(false);
        let namespace = namespace_for_process(Some("soldr-owned".into()), false, || {
            hashed.set(true);
            Ok(hash(0xaa))
        })
        .unwrap();

        assert_eq!(namespace, None);
        assert!(
            !hashed.get(),
            "an inherited value must avoid per-wrapper hashing"
        );
    }

    #[test]
    fn official_release_keeps_the_bare_namespace_without_hashing() {
        let hashed = Cell::new(false);
        let namespace = namespace_for_process(None, true, || {
            hashed.set(true);
            Ok(hash(0xaa))
        })
        .unwrap();

        assert_eq!(namespace, None);
        assert!(!hashed.get(), "official releases retain upgrade semantics");
    }

    #[test]
    fn development_build_uses_version_and_first_sixteen_hash_digits() {
        let namespace = namespace_for_process(None, false, || Ok(hash(0xab))).unwrap();

        assert_eq!(
            namespace.as_deref(),
            Some(concat!(env!("CARGO_PKG_VERSION"), "-abababababababab"))
        );
    }

    #[test]
    fn empty_inherited_namespace_is_not_treated_as_an_identity() {
        let namespace = namespace_for_process(Some("  ".into()), false, || Ok(hash(0x12))).unwrap();

        assert_eq!(
            namespace.as_deref(),
            Some(concat!(env!("CARGO_PKG_VERSION"), "-1212121212121212"))
        );
    }

    #[test]
    fn development_hash_failure_is_not_silently_downgraded() {
        let error = namespace_for_process(None, false, || {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "locked"))
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }
}

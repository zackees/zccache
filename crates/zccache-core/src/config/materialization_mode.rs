//! `ZCCACHE_MODE`: how a cache hit is delivered to its requested path (#1683).
//!
//! - `AUTO` (default): reflink where the cache and target share a
//!   reflink-capable volume, else hardlink (only for outputs whose delivery
//!   policy allows sharing an inode), else copy.
//! - `LINK`: hardlink eligible outputs; outputs the policy forbids sharing
//!   are demoted to the independent reflink-else-copy ladder.
//! - `COPY`: always an independent, writable byte copy. Never probes.
//! - `REFLINK`: an independent, writable copy-on-write clone; falls back to
//!   `COPY` (never to `LINK`) where the volume cannot reflink.
//!
//! `COPY` and `REFLINK` deliver the same thing — an independent, writable
//! inode carrying the cache file's mtime — and differ only in the syscall.
//!
//! This module is the single owner of the variable's name and grammar; a
//! guard test rejects raw reads of `ZCCACHE_MODE` anywhere else.

use std::fmt;
use std::str::FromStr;

/// Environment variable selecting the [`MaterializationMode`].
pub const MATERIALIZATION_MODE_ENV: &str = "ZCCACHE_MODE";

/// How a cached artifact is delivered to its requested output path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum MaterializationMode {
    /// Reflink, else hardlink (policy permitting), else copy.
    #[default]
    Auto,
    /// Hardlink policy-eligible outputs; others take reflink-else-copy.
    Link,
    /// Always an independent byte copy.
    Copy,
    /// Independent copy-on-write clone, falling back to a copy.
    Reflink,
}

impl MaterializationMode {
    /// Every mode, in documentation order.
    pub const ALL: [Self; 4] = [Self::Auto, Self::Link, Self::Copy, Self::Reflink];

    /// Canonical upper-case spelling, as documented and as `Display` prints.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "AUTO",
            Self::Link => "LINK",
            Self::Copy => "COPY",
            Self::Reflink => "REFLINK",
        }
    }

    /// Parse one configured value. Case and surrounding whitespace are
    /// ignored. An empty (or all-whitespace) value is *unset* (`Ok(None)`),
    /// so a lower-precedence source still applies; any other unrecognised
    /// value is an error rather than a silent `AUTO`.
    pub fn parse(raw: &str) -> Result<Option<Self>, InvalidMaterializationMode> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        Self::ALL
            .into_iter()
            .find(|mode| trimmed.eq_ignore_ascii_case(mode.as_str()))
            .map(Some)
            .ok_or_else(|| InvalidMaterializationMode {
                value: raw.to_string(),
            })
    }
}

/// Which sharing tiers a delivery may try, in the fixed order reflink ->
/// hardlink; an independent byte copy always follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaterializationTiers {
    pub reflink: bool,
    pub hardlink: bool,
}

impl MaterializationMode {
    /// Tiers for a delivery with no per-output policy restriction (the
    /// cache store, `zccache warm`, rust-plan bundles), before volume
    /// capabilities are known. LINK never clones; COPY neither clones nor
    /// links; REFLINK never links.
    #[must_use]
    pub const fn tiers_for_shareable(self) -> MaterializationTiers {
        MaterializationTiers {
            reflink: matches!(self, Self::Auto | Self::Reflink),
            hardlink: matches!(self, Self::Auto | Self::Link),
        }
    }
}

impl MaterializationMode {
    /// The copy tier for this mode. `COPY` writes every byte itself so the
    /// destination owns its blocks: `std::fs::copy` uses `copy_file_range`,
    /// which btrfs and XFS may satisfy with a clone, silently turning COPY
    /// into REFLINK. Every other mode keeps the kernel's fast path, where a
    /// shared-extent result is acceptable.
    ///
    /// The COPY path creates `destination` exclusively (callers remove it
    /// first): opening an existing path could truncate a file that a racing
    /// delivery has just hardlinked to the cache blob. A failed copy removes
    /// its partial destination. Permissions — and on Windows, where
    /// `CopyFileExW` preserves it, the modification time — match what
    /// `std::fs::copy` produces.
    pub fn copy_file(
        self,
        source: &std::path::Path,
        destination: &std::path::Path,
    ) -> std::io::Result<u64> {
        if self != Self::Copy {
            return std::fs::copy(source, destination);
        }
        byte_copy(source, destination)
    }
}

/// COPY's copy tier: every byte through userspace, no `copy_file_range`.
fn byte_copy(source: &std::path::Path, destination: &std::path::Path) -> std::io::Result<u64> {
    let mut reader = std::fs::File::open(source)?;
    let metadata = reader.metadata()?;
    let writer = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    // Only a file this call created is removed on failure.
    let result = fill(&mut reader, writer, &metadata, destination);
    if result.is_err() {
        let _ = std::fs::remove_file(destination);
    }
    result
}

fn fill(
    reader: &mut std::fs::File,
    mut writer: std::fs::File,
    metadata: &std::fs::Metadata,
    destination: &std::path::Path,
) -> std::io::Result<u64> {
    use std::io::{Read, Write};
    let mut buffer = vec![0_u8; 256 * 1024];
    let mut copied = 0_u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        writer.write_all(&buffer[..read])?;
        copied += read as u64;
    }
    writer.flush()?;
    #[cfg(windows)]
    writer.set_modified(metadata.modified()?)?;
    drop(writer);
    std::fs::set_permissions(destination, metadata.permissions())?;
    Ok(copied)
}

impl fmt::Display for MaterializationMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MaterializationMode {
    type Err = InvalidMaterializationMode;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse(raw)?.ok_or_else(|| InvalidMaterializationMode {
            value: raw.to_string(),
        })
    }
}

/// A `ZCCACHE_MODE` value that is not one of the four modes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidMaterializationMode {
    value: String,
}

impl InvalidMaterializationMode {
    /// The rejected value, verbatim.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl fmt::Display for InvalidMaterializationMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid {MATERIALIZATION_MODE_ENV} value {:?}: expected one of AUTO, LINK, COPY, REFLINK",
            self.value
        )
    }
}

impl std::error::Error for InvalidMaterializationMode {}

/// Parse an optional configured value (e.g. a request environment entry).
pub fn parse_materialization_mode(
    value: Option<&str>,
) -> Result<Option<MaterializationMode>, InvalidMaterializationMode> {
    value.map_or(Ok(None), MaterializationMode::parse)
}

/// Read `ZCCACHE_MODE` from this process's environment.
///
/// A value that is not valid Unicode is reported as invalid.
pub fn materialization_mode_from_env(
) -> Result<Option<MaterializationMode>, InvalidMaterializationMode> {
    match std::env::var(MATERIALIZATION_MODE_ENV) {
        Ok(value) => MaterializationMode::parse(&value),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(value)) => Err(InvalidMaterializationMode {
            value: value.to_string_lossy().into_owned(),
        }),
    }
}

/// Look up `ZCCACHE_MODE` in a forwarded client environment.
pub fn materialization_mode_from_client_env(
    client_env: Option<&[(String, String)]>,
) -> Result<Option<MaterializationMode>, InvalidMaterializationMode> {
    parse_materialization_mode(client_env.and_then(|env| {
        env.iter()
            .find_map(|(key, value)| (key == MATERIALIZATION_MODE_ENV).then_some(value.as_str()))
    }))
}

#[cfg(test)]
#[path = "materialization_mode_tests.rs"]
mod tests;

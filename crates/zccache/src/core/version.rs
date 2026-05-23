//! Lightweight semver parser for `major.minor.patch` version strings.
//!
//! No external dependencies — versions from Cargo.toml are always clean
//! three-component strings, so a 10-line parser is all we need.

/// A parsed `major.minor.patch` version.
///
/// `Ord` is derived over `(major, minor, patch)` which gives correct
/// semver ordering for our use case (no pre-release tags).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    /// Parse a `"major.minor.patch"` string. Returns `None` on any
    /// malformed input (wrong number of components, non-numeric, etc.).
    pub fn parse(s: &str) -> Option<Self> {
        let mut parts = s.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None; // reject "1.2.3.4"
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

/// Return the version of the currently compiled crate.
pub fn current() -> Version {
    Version::parse(super::VERSION).expect("CARGO_PKG_VERSION is not valid semver")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid() {
        assert_eq!(
            Version::parse("1.2.3"),
            Some(Version {
                major: 1,
                minor: 2,
                patch: 3,
            })
        );
    }

    #[test]
    fn parse_zeros() {
        assert_eq!(
            Version::parse("0.0.0"),
            Some(Version {
                major: 0,
                minor: 0,
                patch: 0,
            })
        );
    }

    #[test]
    fn parse_large_numbers() {
        assert_eq!(
            Version::parse("100.200.300"),
            Some(Version {
                major: 100,
                minor: 200,
                patch: 300,
            })
        );
    }

    #[test]
    fn parse_rejects_empty() {
        assert_eq!(Version::parse(""), None);
    }

    #[test]
    fn parse_rejects_one_component() {
        assert_eq!(Version::parse("1"), None);
    }

    #[test]
    fn parse_rejects_two_components() {
        assert_eq!(Version::parse("1.2"), None);
    }

    #[test]
    fn parse_rejects_four_components() {
        assert_eq!(Version::parse("1.2.3.4"), None);
    }

    #[test]
    fn parse_rejects_non_numeric() {
        assert_eq!(Version::parse("1.2.beta"), None);
    }

    #[test]
    fn parse_rejects_negative() {
        assert_eq!(Version::parse("1.-2.3"), None);
    }

    #[test]
    fn ordering_by_patch() {
        let v1 = Version::parse("1.0.1").unwrap();
        let v2 = Version::parse("1.0.2").unwrap();
        assert!(v1 < v2);
    }

    #[test]
    fn ordering_by_minor() {
        let v1 = Version::parse("1.1.9").unwrap();
        let v2 = Version::parse("1.2.0").unwrap();
        assert!(v1 < v2);
    }

    #[test]
    fn ordering_by_major() {
        let v1 = Version::parse("1.9.9").unwrap();
        let v2 = Version::parse("2.0.0").unwrap();
        assert!(v1 < v2);
    }

    #[test]
    fn ordering_equal() {
        let v1 = Version::parse("1.2.3").unwrap();
        let v2 = Version::parse("1.2.3").unwrap();
        assert_eq!(v1, v2);
        assert!(v1 >= v2);
        assert!(v1 <= v2);
    }

    #[test]
    fn current_version_parses() {
        let v = current();
        // Just verify it parsed — the exact value depends on Cargo.toml
        assert!(v.major > 0 || v.minor > 0 || v.patch > 0);
    }
}

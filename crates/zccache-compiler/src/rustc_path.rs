//! Lexical path syntax supplied independently of compiler output naming.
use typed_path::{Utf8Component, Utf8UnixPath, Utf8WindowsPath};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Embedding path syntax, independent of Rustc host or requested target.
pub enum RustcPathSyntax {
    /// Unix separators and components.
    Unix,
    /// Windows separators, prefixes, and components.
    Windows,
}

impl RustcPathSyntax {
    #[cfg(feature = "native")]
    pub(crate) fn current() -> Self {
        if crate::platform::host::is_windows() {
            Self::Windows
        } else {
            Self::Unix
        }
    }
    pub(crate) fn file_stem(self, path: &str) -> Option<&str> {
        match self {
            Self::Unix => Utf8UnixPath::new(path).file_stem(),
            Self::Windows => Utf8WindowsPath::new(path).file_stem(),
        }
    }

    pub(crate) fn is_dylint_linker(self, path: Option<&str>) -> bool {
        path.and_then(|path| self.file_stem(path))
            .is_some_and(|stem| stem.eq_ignore_ascii_case("dylint-link"))
    }

    pub(crate) fn is_dylint_library_dir(self, path: Option<&str>) -> bool {
        let Some(path) = path else {
            return false;
        };
        match self {
            Self::Unix => adjacent(
                Utf8UnixPath::new(path)
                    .components()
                    .map(|part| part.as_str()),
            ),
            Self::Windows => adjacent(
                Utf8WindowsPath::new(path)
                    .components()
                    .map(|part| part.as_str()),
            ),
        }
    }
}

fn adjacent<'a>(parts: impl Iterator<Item = &'a str>) -> bool {
    let mut previous = "";
    for part in parts {
        if previous == "dylint" && part == "libraries" {
            return true;
        }
        previous = part;
    }
    false
}

#[cfg(feature = "native")]
#[cfg(test)]
mod tests {
    use super::RustcPathSyntax;
    #[test]
    fn native_lexical_equivalence() {
        let syntax = RustcPathSyntax::current();
        for path in [
            "",
            ".",
            "..",
            "/",
            "./fixture.rs",
            "a/../fixture.rs",
            "a//fixture.rs/.",
            ".hidden",
            "name.",
            "dylint/./libraries/out",
            "dylint/../libraries",
            "dylint//libraries",
            r"C:\src\fixture.rs",
            r"\\server\share\dylint\libraries",
            r"\\?\C:\src\fixture.rs",
            "/tools/DYLINT-LINK.exe",
            r"C:fixture.rs",
        ] {
            let native = std::path::Path::new(path);
            assert_eq!(
                syntax.file_stem(path),
                native.file_stem().and_then(|s| s.to_str()),
                "{path:?}"
            );
            let components: Vec<_> = native.components().collect();
            let expected = components
                .windows(2)
                .any(|pair| pair[0].as_os_str() == "dylint" && pair[1].as_os_str() == "libraries");
            assert_eq!(
                syntax.is_dylint_library_dir(Some(path)),
                expected,
                "{path:?}"
            );
        }
    }
}

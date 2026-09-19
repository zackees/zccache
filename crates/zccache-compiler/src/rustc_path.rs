//! Lexical path syntax supplied independently of compiler output naming.
use typed_path::{Utf8UnixPath, Utf8WindowsPath};

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
        if kernal_api::platform::host::target_is_windows() {
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

    /// `-C linker=dylint-link` — the one signal that identifies a Dylint
    /// lint cdylib.
    ///
    /// There is deliberately no companion output-tree predicate
    /// (zackees/soldr#3044). Dylint installs this linker only for its own
    /// lint libraries, but it writes them into more than one tree
    /// (`dylint/libraries`, and `dylint/tests/<name>/target/...` for a
    /// lint's own test crate), so an adjacency check on the out-dir
    /// rejected real Dylint cdylibs while adding nothing the linker name
    /// did not already establish.
    pub(crate) fn is_dylint_linker(self, path: Option<&str>) -> bool {
        path.and_then(|path| self.file_stem(path))
            .is_some_and(|stem| stem.eq_ignore_ascii_case("dylint-link"))
    }
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
            // `is_dylint_linker` is a pure file-stem predicate, so native
            // equivalence of `file_stem` above is what makes it portable.
            let expected = native
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| stem.eq_ignore_ascii_case("dylint-link"));
            assert_eq!(syntax.is_dylint_linker(Some(path)), expected, "{path:?}");
        }
    }

    /// zackees/soldr#3044: the Dylint cdylib decision is keyed on the
    /// linker alone, so the out-dir tree is irrelevant to it. This is the
    /// lexical half of that contract — no path-adjacency predicate exists
    /// to reject `dylint/tests` (or any other tree) any more.
    #[test]
    fn dylint_linker_detection_ignores_the_output_tree() {
        for syntax in [RustcPathSyntax::Unix, RustcPathSyntax::Windows] {
            assert!(syntax.is_dylint_linker(Some("/tools/dylint-link")));
            assert!(syntax.is_dylint_linker(Some("/tools/DYLINT-LINK.exe")));
            assert!(!syntax.is_dylint_linker(Some("/tools/cc")));
            assert!(!syntax.is_dylint_linker(None));
        }
    }
}

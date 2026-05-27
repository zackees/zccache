//! Pure wrapper routing decisions.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WrapperRoute {
    Formatter,
    LinkOrArchive,
    Compile,
}

pub(super) fn classify_invocation(tool: &str, tool_args: &[String]) -> WrapperRoute {
    if crate::compiler::detect_family(tool).is_formatter() {
        return WrapperRoute::Formatter;
    }

    if crate::compiler::parse_archiver::is_archiver(tool)
        || crate::compiler::parse_linker::is_link_invocation(tool, tool_args)
    {
        return WrapperRoute::LinkOrArchive;
    }

    WrapperRoute::Compile
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn routes_rustfmt_to_formatter() {
        assert_eq!(
            classify_invocation("rustfmt", &args(&["src/lib.rs"])),
            WrapperRoute::Formatter
        );
    }

    #[test]
    fn routes_archiver_to_link_or_archive() {
        assert_eq!(
            classify_invocation("ar", &args(&["rcs", "libfoo.a", "foo.o"])),
            WrapperRoute::LinkOrArchive
        );
    }

    #[test]
    fn routes_shared_linker_invocation_to_link_or_archive() {
        assert_eq!(
            classify_invocation("gcc", &args(&["-shared", "foo.o", "-o", "libfoo.so"])),
            WrapperRoute::LinkOrArchive
        );
    }

    #[test]
    fn routes_regular_compiler_invocation_to_compile() {
        assert_eq!(
            classify_invocation("rustc", &args(&["--crate-name", "demo", "src/lib.rs"])),
            WrapperRoute::Compile
        );
    }
}

//! Linker key material for rustc products whose bytes the linker shapes.
//!
//! The rustc context key drops `-C linker` and `-C link-arg(s)` because they
//! do not change an rlib/rmeta. Two linked products zccache admits do depend
//! on them, so their keys fold the linker identity and every link argument
//! back in:
//!
//! - the Dylint lint cdylib (soldr#2349), whose linker is always explicit;
//! - an admitted rustc `--test` harness (zccache#1550), where a changed link
//!   argument or linker must never be served a previously linked executable.

use crate::depgraph::RustcParsedArgs;
use crate::hash::ContentHash;

/// A linked product whose key carries linker inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LinkedProduct {
    DylintCdylib,
    TestHarness,
}

/// Which linked product `args` builds, if its key needs linker inputs.
pub(super) fn linked_product(args: &RustcParsedArgs) -> Option<LinkedProduct> {
    if super::is_dylint_cdylib_args(args) {
        Some(LinkedProduct::DylintCdylib)
    } else if args.unknown_flags.iter().any(|flag| flag == "--test") {
        Some(LinkedProduct::TestHarness)
    } else {
        None
    }
}

/// Fold the linker identity (`None`: the target's default linker) and every
/// `-C link-arg(s)` into the key's ordered codegen flags.
pub(super) fn add_link_key_material(
    args: &mut RustcParsedArgs,
    product: LinkedProduct,
    linker_hash: Option<ContentHash>,
) {
    let identity = match (product, linker_hash) {
        (LinkedProduct::DylintCdylib, hash) => format!(
            "dylint-linker-hash={}",
            hash.unwrap_or(super::COMPILER_HASH_UNAVAILABLE)
        ),
        (LinkedProduct::TestHarness, Some(hash)) => format!("test-harness-linker-hash={hash}"),
        (LinkedProduct::TestHarness, None) => "test-harness-linker=default".to_string(),
    };
    args.codegen_flags.push(identity);
    args.codegen_flags.extend(args.linker_args.iter().cloned());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn parse(args: &[&str]) -> RustcParsedArgs {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
        crate::depgraph::parse_rustc_args(&args, Path::new("/w"))
    }

    #[test]
    fn only_harnesses_and_dylint_cdylibs_carry_linker_inputs() {
        assert_eq!(
            linked_product(&parse(&["--test", "src/lib.rs"])),
            Some(LinkedProduct::TestHarness)
        );
        assert_eq!(
            linked_product(&parse(&["--crate-type", "lib", "src/lib.rs"])),
            None
        );
    }

    #[test]
    fn a_harness_key_differs_by_link_argument_and_linker() {
        let keyed = |args: &[&str], hash: Option<ContentHash>| {
            let mut parsed = parse(args);
            add_link_key_material(&mut parsed, LinkedProduct::TestHarness, hash);
            parsed.codegen_flags
        };
        let base = keyed(&["--test", "src/lib.rs"], None);
        let link_arg = keyed(&["--test", "-C", "link-arg=-Wl,-O1", "src/lib.rs"], None);
        let explicit = keyed(
            &["--test", "src/lib.rs"],
            Some(ContentHash::from_bytes([7; 32])),
        );
        assert_ne!(base, link_arg);
        assert_ne!(base, explicit);
        assert_ne!(link_arg, explicit);
    }
}

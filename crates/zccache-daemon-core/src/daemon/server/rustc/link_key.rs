//! Linker key material for rustc products whose bytes the linker shapes.
//!
//! The rustc context key drops `-C linker` and `-C link-arg(s)` because they
//! do not change an rlib/rmeta. The linked products zccache admits do depend
//! on them, so their keys fold the linker identity and every link argument
//! back in:
//!
//! - the Dylint lint cdylib (soldr#2349), whose linker is always explicit;
//! - an admitted rustc `--test` harness (zccache#1550), where a changed link
//!   argument or linker must never be served a previously linked executable;
//! - an ordinary cargo `--crate-type bin` / `--crate-type staticlib` unit,
//!   and a bare invocation that takes rustc's `bin` default because it passed
//!   no `--crate-type` at all (zccache#1900). Those crate types are cacheable
//!   *and* linker-produced, so switching either flag previously served the
//!   previously linked artifact.
//!
//! `dylib`/`cdylib` stay out on purpose — they are not cached — and a `lib` /
//! `rlib` / `proc-macro` unit must keep returning `None` here: over-keying an
//! rlib on linker inputs would cost cache hits for no correctness gain.

use crate::depgraph::RustcParsedArgs;
use crate::hash::ContentHash;

/// A linked product whose key carries linker inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LinkedProduct {
    DylintCdylib,
    TestHarness,
    /// An ordinary cargo `bin` / `staticlib` unit (zccache#1900).
    ///
    /// These crate types are cacheable *and* their output bytes are produced
    /// by the linker, so the key folds the linker identity and every
    /// `-C link-arg(s)` exactly like the harness lane above. Without that, a
    /// build that switched `-C linker=` or `-C link-arg=-Wl,...` between two
    /// invocations was served the previously linked artifact.
    CargoLinkedBinary,
}

/// Whether these declared crate types (or their absence) mean the linker
/// produces this unit's output bytes.
///
/// An argv with no `--crate-type` at all is rustc's `bin` default, and the
/// admission side admits it on exactly that basis
/// (`RUSTC_CACHEABLE_CRATE_TYPES` plus the default-to-`bin` fallback in
/// `zccache-compiler`'s `parse_rustc_plan`). `parse_rustc_args` leaves the
/// vector empty instead of filling that default in, so the default has to be
/// applied here — otherwise the most common `bin` shape, a bare
/// `rustc src/main.rs` and the repro in zccache#1900, is the one linked
/// product that still drops `-C linker=` / `-C link-arg(s)`.
fn links_output(crate_types: &[String]) -> bool {
    crate_types.is_empty()
        || crate_types
            .iter()
            .any(|ct| matches!(ct.as_str(), "bin" | "staticlib"))
}

/// Which linked product `args` builds, if its key needs linker inputs.
pub(super) fn linked_product(args: &RustcParsedArgs) -> Option<LinkedProduct> {
    if super::is_dylint_cdylib_args(args) {
        Some(LinkedProduct::DylintCdylib)
    } else if args.unknown_flags.iter().any(|flag| flag == "--test") {
        Some(LinkedProduct::TestHarness)
    } else if links_output(&args.crate_types) {
        Some(LinkedProduct::CargoLinkedBinary)
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
        // Distinct from the harness markers above so a `bin` unit and a
        // `--test` unit compiled from the same sources with the same linker
        // never share one key material shape.
        (LinkedProduct::CargoLinkedBinary, hash) => format!(
            "cargo-linked-binary-linker-hash={}",
            hash.unwrap_or(super::COMPILER_HASH_UNAVAILABLE)
        ),
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
    fn only_linked_products_carry_linker_inputs() {
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
    fn cargo_linked_binaries_carry_linker_inputs_but_rlibs_do_not() {
        // zccache#1900: `bin`/`staticlib` units are cacheable *and* linker
        // produced, so their key must fold `-C linker=` / `-C link-arg=`.
        assert_eq!(
            linked_product(&parse(&["--crate-type", "bin", "src/main.rs"])),
            Some(LinkedProduct::CargoLinkedBinary)
        );
        assert_eq!(
            linked_product(&parse(&["--crate-type", "staticlib", "src/lib.rs"])),
            Some(LinkedProduct::CargoLinkedBinary)
        );
        // An rlib must NOT start carrying linker inputs: over-keying it costs
        // cache hits for no correctness gain.
        assert_eq!(
            linked_product(&parse(&["--crate-type", "lib", "src/lib.rs"])),
            None
        );
        assert_eq!(
            linked_product(&parse(&["--crate-type", "rlib", "src/lib.rs"])),
            None
        );
        // `--test` overrides the crate type and keeps its own lane.
        assert_eq!(
            linked_product(&parse(&["--test", "--crate-type", "bin", "src/main.rs"])),
            Some(LinkedProduct::TestHarness)
        );
    }

    #[test]
    fn an_invocation_with_no_crate_type_takes_rustcs_bin_default() {
        // zccache#1900's repro passes no `--crate-type`, and the admission
        // side defaults that shape to a cacheable `bin`. It links, so it must
        // carry linker inputs too — otherwise the reported collision survives
        // for the most common `bin` invocation there is.
        assert_eq!(
            linked_product(&parse(&["src/main.rs"])),
            Some(LinkedProduct::CargoLinkedBinary)
        );
        assert_eq!(
            linked_product(&parse(&["-C", "link-arg=-Wl,-O1", "src/main.rs"])),
            Some(LinkedProduct::CargoLinkedBinary)
        );
    }

    #[test]
    fn a_cargo_linked_binary_key_differs_by_link_argument_and_linker() {
        let keyed = |args: &[&str], hash: Option<ContentHash>| {
            let mut parsed = parse(args);
            add_link_key_material(&mut parsed, LinkedProduct::CargoLinkedBinary, hash);
            parsed.codegen_flags
        };
        let base = keyed(&["--crate-type", "bin", "src/main.rs"], None);
        let link_arg = keyed(
            &[
                "--crate-type",
                "bin",
                "-C",
                "link-arg=-Wl,-O1",
                "src/main.rs",
            ],
            None,
        );
        let explicit = keyed(
            &["--crate-type", "bin", "src/main.rs"],
            Some(ContentHash::from_bytes([7; 32])),
        );
        assert_ne!(base, link_arg);
        assert_ne!(base, explicit);
        assert_ne!(link_arg, explicit);
    }

    #[test]
    fn cargo_linked_binary_material_is_distinct_from_the_harness() {
        let mut binary = parse(&["--crate-type", "bin", "src/main.rs"]);
        add_link_key_material(&mut binary, LinkedProduct::CargoLinkedBinary, None);
        let mut harness = parse(&["--test", "src/lib.rs"]);
        add_link_key_material(&mut harness, LinkedProduct::TestHarness, None);
        assert_ne!(binary.codegen_flags, harness.codegen_flags);
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

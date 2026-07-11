//! Semantic output classification used by the immutable-output rollout.
//!
//! The classification deliberately does not infer safety from a filename
//! alone.  The parser supplies the compiler family and invocation mode; this
//! module turns that information into a conservative delivery policy.

use crate::CompilerFamily;

/// The logical role of a compiler-produced file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputRole {
    /// Rust metadata consumed as a read-only interface.
    RustMetadata,
    /// Rust archive (`.rlib` or static library).
    RustArchive,
    /// A native compiler object file.
    Object,
    /// A dependency file consumed by a build system.
    DepInfo,
    /// A precompiled header or module artifact.
    PrecompiledHeaderOrModule,
    /// A program or shared-library output.
    ExecutableOrLibrary,
    /// A linker/debug side output whose lifecycle is compiler-specific.
    LinkerSideOutput,
    /// An output whose producer/consumer contract is not proven.
    MutableOpaque,
}

/// What a producer and its consumers promise about mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationContract {
    /// Consumers do not edit bytes and the producer replaces the path.
    ImmutableConsumer,
    /// The producer writes a new file by replacement, never in place.
    AtomicReplaceOnly,
    /// A consumer or producer may edit/truncate the file in place.
    MayEditInPlace,
    /// No reliable contract is known.
    Unknown,
}

/// Delivery options for a published immutable object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryPolicy {
    /// Reflink is preferred; a full copy is the safe fallback.
    ReflinkPreferred,
    /// Hardlink is allowed only after the semantic allowlist and platform
    /// lifecycle tests pass.  It is not COW.
    HardlinkEligible,
    /// Never share an inode with the requested output.
    IndependentOnly,
}

/// A complete semantic classification for one output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputClassification {
    pub role: OutputRole,
    pub mutation: MutationContract,
    pub delivery: DeliveryPolicy,
}

impl OutputClassification {
    /// Conservative classification for a parsed compiler invocation.
    #[must_use]
    pub fn for_compiler(family: CompilerFamily, output_name: &str) -> Self {
        let extension = output_name
            .rsplit_once('.')
            .map_or("", |(_, extension)| extension);
        match family {
            CompilerFamily::Rustc if matches!(extension, "rmeta") => Self {
                role: OutputRole::RustMetadata,
                mutation: MutationContract::ImmutableConsumer,
                delivery: DeliveryPolicy::HardlinkEligible,
            },
            CompilerFamily::Rustc if matches!(extension, "rlib") => Self {
                role: OutputRole::RustArchive,
                mutation: MutationContract::ImmutableConsumer,
                delivery: DeliveryPolicy::HardlinkEligible,
            },
            CompilerFamily::Gcc | CompilerFamily::Clang | CompilerFamily::Msvc
                if extension == "o" || extension == "obj" =>
            {
                Self {
                    role: OutputRole::Object,
                    mutation: MutationContract::AtomicReplaceOnly,
                    delivery: DeliveryPolicy::ReflinkPreferred,
                }
            }
            CompilerFamily::Gcc | CompilerFamily::Clang | CompilerFamily::Msvc
                if matches!(extension, "pch" | "gch" | "pcm") =>
            {
                Self {
                    role: OutputRole::PrecompiledHeaderOrModule,
                    mutation: MutationContract::AtomicReplaceOnly,
                    delivery: DeliveryPolicy::ReflinkPreferred,
                }
            }
            _ if extension == "d" => Self {
                role: OutputRole::DepInfo,
                mutation: MutationContract::MayEditInPlace,
                delivery: DeliveryPolicy::IndependentOnly,
            },
            _ if matches!(extension, "pch" | "gch" | "pcm") => Self {
                role: OutputRole::PrecompiledHeaderOrModule,
                mutation: MutationContract::Unknown,
                delivery: DeliveryPolicy::IndependentOnly,
            },
            _ => Self {
                role: if matches!(
                    family,
                    CompilerFamily::Gcc | CompilerFamily::Clang | CompilerFamily::Msvc
                ) {
                    OutputRole::ExecutableOrLibrary
                } else {
                    OutputRole::MutableOpaque
                },
                mutation: MutationContract::Unknown,
                delivery: DeliveryPolicy::IndependentOnly,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_proven_rust_interfaces_are_hardlink_candidates() {
        assert_eq!(
            OutputClassification::for_compiler(CompilerFamily::Rustc, "libx.rlib").delivery,
            DeliveryPolicy::HardlinkEligible
        );
        assert_eq!(
            OutputClassification::for_compiler(CompilerFamily::Rustc, "x.exe").delivery,
            DeliveryPolicy::IndependentOnly
        );
        assert_eq!(
            OutputClassification::for_compiler(CompilerFamily::Clang, "x.o").delivery,
            DeliveryPolicy::ReflinkPreferred
        );
        assert_eq!(
            OutputClassification::for_compiler(CompilerFamily::Clang, "x.pch").role,
            OutputRole::PrecompiledHeaderOrModule
        );
    }

    #[test]
    fn depfiles_and_unknown_outputs_are_never_shared() {
        assert_eq!(
            OutputClassification::for_compiler(CompilerFamily::Gcc, "x.d").delivery,
            DeliveryPolicy::IndependentOnly
        );
        assert_eq!(
            OutputClassification::for_compiler(CompilerFamily::Gcc, "custom.output").delivery,
            DeliveryPolicy::IndependentOnly
        );
    }
}

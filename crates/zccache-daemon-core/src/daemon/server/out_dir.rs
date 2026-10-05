//! Audited rustc `OUT_DIR` uses whose path is only an input lookup location.
//!
//! Rustc dep-info records the value of `env!("OUT_DIR")`, but not its use site.
//! Normalizing it for arbitrary crates would serve stale artifacts when a crate
//! embeds the path itself (#1021). Keep this certificate narrow and fail closed.

use super::*;
use sha2::{Digest, Sha256};

enum SourceAttestation<'a> {
    Listed(&'a [(&'a str, &'a str)]),
    RustTreeSha256(&'a str),
}

struct PathOnlyCertificate<'a> {
    package_name: &'a str,
    version: &'a str,
    sources: SourceAttestation<'a>,
    generated_name: &'a str,
    generated_sha256: &'a str,
    required_includes: &'a [&'a str],
    cache_value: &'a str,
}

#[derive(Debug)]
pub(super) struct CertifiedOutDir {
    pub(super) path: String,
    pub(super) generated_name: &'static str,
}

const PATH_ONLY_OUT_DIR_VALUE: &str = "zccache:path-only-out-dir:libsqlite3-sys-0.30.1:v1";
const LIBSQLITE3_SYS_SOURCES: &[(&str, &str)] = &[
    (
        "src/lib.rs",
        "7efb61223be16f97bb65c4541e0c37b97c624b06356d18041654e30b27fb4114",
    ),
    (
        "src/error.rs",
        "c09656237b7e51eec9cac261de83555e60c401071eeaecb7649f8e94b769c156",
    ),
    (
        "build.rs",
        "11bebc3787657749279375ab2e875e52259f995dce8916b4907103975e2a76ee",
    ),
];
const LIBSQLITE3_SYS_CERTIFICATE: PathOnlyCertificate<'static> = PathOnlyCertificate {
    package_name: "libsqlite3-sys",
    version: "0.30.1",
    sources: SourceAttestation::Listed(LIBSQLITE3_SYS_SOURCES),
    generated_name: "bindgen.rs",
    generated_sha256: "8a77e8d566d1637a551ab4e35837e016f6132aa64e9c92fd07b1039898734559",
    required_includes: &["src/error.rs"],
    cache_value: PATH_ONLY_OUT_DIR_VALUE,
};
const SERDE_CORE_CERTIFICATE: PathOnlyCertificate<'static> = PathOnlyCertificate {
    package_name: "serde_core",
    version: "1.0.228",
    sources: SourceAttestation::RustTreeSha256(
        "cee65f0f50fc6839a93d386652a2c5b75ad44df80c80541b9dba892749042ba1",
    ),
    generated_name: "private.rs",
    generated_sha256: "27ad1b5fb4eebeb1da6419cbfad3ac1922ed1d817fcbab0a1dae1bdfd38d8307",
    required_includes: &[],
    cache_value: "zccache:path-only-out-dir:serde_core-1.0.228:v1",
};
const SERDE_CERTIFICATE: PathOnlyCertificate<'static> = PathOnlyCertificate {
    package_name: "serde",
    version: "1.0.228",
    sources: SourceAttestation::RustTreeSha256(
        "1e12c2c58a2c172af6b651a6e10e1837259adffdb3e4f44fdab39123562b540b",
    ),
    generated_name: "private.rs",
    generated_sha256: "f8e9470772811a1bdcd201fb21e14cbf37a57d192b38cd98f66bd4a83442c26b",
    required_includes: &[],
    cache_value: "zccache:path-only-out-dir:serde-1.0.228:v1",
};
const CERTIFICATES: &[PathOnlyCertificate<'static>] = &[
    LIBSQLITE3_SYS_CERTIFICATE,
    SERDE_CORE_CERTIFICATE,
    SERDE_CERTIFICATE,
];

/// Value used only for zccache artifact identity. The caller must still pass
/// the physical `OUT_DIR` to rustc and preserve its physical dep-info for Cargo.
pub(super) fn rustc_env_dep_cache_value(
    client_env: Option<&[(String, String)]>,
    name: &str,
    source_path: &Path,
    resolved_includes: &[NormalizedPath],
) -> Option<String> {
    if name == "OUT_DIR" {
        if let Some(certificate) = matching_certificate(client_env, source_path, resolved_includes)
        {
            return Some(certificate.cache_value.to_string());
        }
    }
    rustc_env_dep_value(client_env, name).map(str::to_owned)
}

/// Physical path to replay in dep-info after a certified hit. A failed or
/// absent certificate must not authorize any dep-info transformation.
pub(super) fn certified_rustc_out_dir(
    state: &SharedState,
    context_key: &ContextKey,
    client_env: Option<&[(String, String)]>,
    source_path: &Path,
) -> Option<CertifiedOutDir> {
    let includes = state.dep_graph.load().get_includes(context_key)?;
    let certificate = matching_certificate(client_env, source_path, &includes)?;
    let path = rustc_env_dep_value(client_env, "OUT_DIR")?.to_owned();
    Some(CertifiedOutDir {
        path,
        generated_name: certificate.generated_name,
    })
}

fn matching_certificate(
    client_env: Option<&[(String, String)]>,
    source_path: &Path,
    resolved_includes: &[NormalizedPath],
) -> Option<&'static PathOnlyCertificate<'static>> {
    CERTIFICATES.iter().find(|certificate| {
        path_only_certificate_matches(client_env, source_path, resolved_includes, certificate)
    })
}

#[cfg(test)]
fn cache_value_with_certificate(
    client_env: Option<&[(String, String)]>,
    name: &str,
    source_path: &Path,
    resolved_includes: &[NormalizedPath],
    certificate: &PathOnlyCertificate<'_>,
) -> Option<String> {
    if name == "OUT_DIR"
        && path_only_certificate_matches(client_env, source_path, resolved_includes, certificate)
    {
        return Some(certificate.cache_value.to_string());
    }
    rustc_env_dep_value(client_env, name).map(str::to_owned)
}

fn path_only_certificate_matches(
    client_env: Option<&[(String, String)]>,
    source_path: &Path,
    resolved_includes: &[NormalizedPath],
    certificate: &PathOnlyCertificate<'_>,
) -> bool {
    if !path_remap_auto_enabled(client_env)
        || client_env_value(client_env, "CARGO_PKG_NAME") != Some(certificate.package_name)
        || client_env_value(client_env, "CARGO_PKG_VERSION") != Some(certificate.version)
    {
        return false;
    }
    let Some(manifest) = client_env_value(client_env, "CARGO_MANIFEST_DIR") else {
        return false;
    };
    let manifest = Path::new(manifest);
    if !manifest.is_absolute()
        || NormalizedPath::new(source_path) != NormalizedPath::new(manifest.join("src/lib.rs"))
    {
        return false;
    }

    // A patched/future crate version must be audited independently. The
    // Serde packages have many source modules, so pin the complete Rust
    // source tree; libsqlite3-sys pins its three relevant files explicitly.
    match &certificate.sources {
        SourceAttestation::Listed(files) => {
            for &(relative, digest) in *files {
                let Ok(bytes) = std::fs::read(manifest.join(relative)) else {
                    return false;
                };
                if format!("{:x}", Sha256::digest(bytes)) != digest {
                    return false;
                }
            }
        }
        SourceAttestation::RustTreeSha256(expected) => {
            if rust_source_tree_sha256(manifest).as_deref() != Some(*expected) {
                return false;
            }
        }
    }

    let Some(out_dir) = rustc_env_dep_value(client_env, "OUT_DIR") else {
        return false;
    };
    let out_dir = Path::new(out_dir);
    if !out_dir.is_absolute() {
        return false;
    }
    let generated = out_dir.join(certificate.generated_name);
    let generated_path = NormalizedPath::new(&generated);
    if !resolved_includes.contains(&generated_path) {
        return false;
    }
    // The generated file can include another module without spelling OUT_DIR.
    // The dep-info rebase handles only the generated path, so every other
    // resolved include must be covered by the attested package sources.
    if resolved_includes.iter().any(|include| {
        if include == &generated_path {
            return false;
        }
        let attested = match &certificate.sources {
            SourceAttestation::Listed(files) => files.iter().any(|(relative, _)| {
                include == &NormalizedPath::new(manifest.join(relative))
            }),
            SourceAttestation::RustTreeSha256(_) => include
                .as_path()
                .strip_prefix(manifest)
                .ok()
                .is_some_and(|relative| {
                    relative.extension().is_some_and(|extension| extension == "rs")
                        && !relative.components().any(|component| {
                            matches!(component, std::path::Component::Normal(name) if name == "target" || name == ".git")
                        })
                }),
        };
        !attested
    }) {
        return false;
    }
    if certificate
        .required_includes
        .iter()
        .any(|relative| !resolved_includes.contains(&NormalizedPath::new(manifest.join(relative))))
    {
        return false;
    }
    let Ok(generated_bytes) = std::fs::read(&generated) else {
        return false;
    };
    // Exact generated output attestation closes lexical bypasses such as
    // `env ! ("OUT\u{5f}DIR")` that a byte blacklist cannot recognize.
    if format!("{:x}", Sha256::digest(&generated_bytes)) != certificate.generated_sha256 {
        return false;
    }
    // The build script may select/generate different bindings. Their bytes
    // remain in the depgraph key, but refuse a generated macro that could
    // read or embed OUT_DIR independently of the audited source line.
    for forbidden in [
        b"OUT_DIR".as_slice(),
        b"env!".as_slice(),
        b"option_env!".as_slice(),
        b"include!".as_slice(),
        b"include_str!".as_slice(),
        b"include_bytes!".as_slice(),
        b"macro_rules!".as_slice(),
    ] {
        if generated_bytes
            .windows(forbidden.len())
            .any(|window| window == forbidden)
        {
            return false;
        }
    }
    true
}

/// Hash each Rust source's slash-normalized relative name and bytes, sorted
/// by name. Symlinks and unreadable files fail closed. This pins every macro
/// definition in the audited Serde releases, not just their crate root.
fn rust_source_tree_sha256(manifest: &Path) -> Option<String> {
    let mut directories = vec![manifest.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(directory).ok()? {
            let entry = entry.ok()?;
            let kind = entry.file_type().ok()?;
            if kind.is_symlink() {
                return None;
            }
            let path = entry.path();
            if kind.is_dir() {
                if entry.file_name() != "target" && entry.file_name() != ".git" {
                    directories.push(path);
                }
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let relative = path.strip_prefix(manifest).ok()?;
                files.push((relative.to_string_lossy().replace('\\', "/"), path));
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = Sha256::new();
    for (relative, path) in files {
        hasher.update(relative.as_bytes());
        hasher.update([0]);
        hasher.update(std::fs::read(path).ok()?);
        hasher.update([0]);
    }
    Some(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::depgraph::{CacheVerdict, IncludeSearchPaths, ScanResult};
    use zccache_hash::hash_bytes;

    fn write(path: &Path, bytes: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        std::fs::write(path, bytes).expect("fixture file");
    }

    fn context(source: &Path) -> CompileContext {
        CompileContext {
            source_file: NormalizedPath::new(source),
            include_search: IncludeSearchPaths::default(),
            defines: Vec::new(),
            undefines: Vec::new(),
            flags: Vec::new(),
            force_includes: Vec::new(),
            unknown_flags: Vec::new(),
            compiler_hash: hash_bytes(b"out-dir-test-compiler"),
        }
    }

    #[test]
    fn serde_style_source_tree_certificate_rejects_modified_source() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("crate");
        let source = root.join("src/lib.rs");
        let generated = root.join("target/out/private.rs");
        write(
            &source,
            "include!(concat!(env!(\"OUT_DIR\"), \"/private.rs\"));\n",
        );
        write(&root.join("build.rs"), "fn main() {}\n");
        write(&generated, "pub const PRIVATE: u32 = 7;\n");
        let generated_digest = format!("{:x}", Sha256::digest(b"pub const PRIVATE: u32 = 7;\n"));
        let tree_digest = rust_source_tree_sha256(&root).expect("source tree digest");
        let certificate = PathOnlyCertificate {
            package_name: "serde-style-test",
            version: "0.1.0",
            sources: SourceAttestation::RustTreeSha256(&tree_digest),
            generated_name: "private.rs",
            generated_sha256: &generated_digest,
            required_includes: &[],
            cache_value: "zccache:path-only-out-dir:serde-style-test:v1",
        };
        let env = vec![
            ("CARGO_PKG_NAME".to_string(), "serde-style-test".to_string()),
            ("CARGO_PKG_VERSION".to_string(), "0.1.0".to_string()),
            (
                "CARGO_MANIFEST_DIR".to_string(),
                root.to_string_lossy().into_owned(),
            ),
            (
                "OUT_DIR".to_string(),
                generated
                    .parent()
                    .expect("out dir")
                    .to_string_lossy()
                    .into_owned(),
            ),
            ("ZCCACHE_PATH_REMAP".to_string(), "auto".to_string()),
        ];
        let includes = [NormalizedPath::new(&generated)];
        assert!(path_only_certificate_matches(
            Some(&env),
            &source,
            &includes,
            &certificate,
        ));
        let sibling = root.join("target/out/sibling.rs");
        write(&generated, "mod sibling;\n");
        write(&sibling, "pub const SIBLING: u32 = 9;\n");
        assert!(
            !path_only_certificate_matches(
                Some(&env),
                &source,
                &[
                    NormalizedPath::new(&generated),
                    NormalizedPath::new(&sibling)
                ],
                &certificate,
            ),
            "a sibling include under OUT_DIR cannot be rebased by this certificate"
        );
        let external = temp.path().join("shared.rs");
        write(&external, "pub const PATH: &str = env!(\"OUT_DIR\");\n");
        write(
            &generated,
            &format!("#[path = \"{}\"] mod shared;\n", external.display()),
        );
        assert!(
            !path_only_certificate_matches(
                Some(&env),
                &source,
                &[
                    NormalizedPath::new(&generated),
                    NormalizedPath::new(&external)
                ],
                &certificate,
            ),
            "an unaudited external module may embed OUT_DIR"
        );
        write(
            &generated,
            r#"pub const PATH: &str = env ! ("OUT\u{5f}DIR");"#,
        );
        assert!(
            !path_only_certificate_matches(Some(&env), &source, &includes, &certificate),
            "generated Rust can spell env! and OUT_DIR without literal substrings"
        );
        write(&source, "pub const PATH: &str = env!(\"OUT_DIR\");\n");
        assert!(!path_only_certificate_matches(
            Some(&env),
            &source,
            &includes,
            &certificate,
        ));
    }

    /// #1749: a certified include-only OUT_DIR use must hit in B, and a
    /// changed generated input must still miss. The same certificate check
    /// is used by the daemon's store, slow-check, and fast-hit paths.
    #[test]
    fn perf_certified_out_dir_include_reuses_sibling_worktree() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = "mod error;\ninclude!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));\n";
        let error = "pub const ERROR: u32 = 1;\n";
        let build = "fn main() {}\n";
        let source_digest = format!("{:x}", Sha256::digest(source.as_bytes()));
        let error_digest = format!("{:x}", Sha256::digest(error.as_bytes()));
        let build_digest = format!("{:x}", Sha256::digest(build.as_bytes()));
        let generated_digest = format!("{:x}", Sha256::digest(b"pub const GENERATED: u32 = 7;\n"));
        let sources = [
            ("src/lib.rs", source_digest.as_str()),
            ("src/error.rs", error_digest.as_str()),
            ("build.rs", build_digest.as_str()),
        ];
        let certificate = PathOnlyCertificate {
            package_name: "out-dir-test",
            version: "0.1.0",
            sources: SourceAttestation::Listed(&sources),
            generated_name: "generated.rs",
            generated_sha256: &generated_digest,
            required_includes: &["src/error.rs"],
            cache_value: "zccache:path-only-out-dir:test:v1",
        };
        let graph = DepGraph::new();
        let key = ContextKey::from_raw([0x49; 32]);
        let mut roots = Vec::new();
        let mut source_paths = Vec::new();
        let mut generated_paths = Vec::new();
        let mut includes = Vec::new();
        let mut environments = Vec::new();
        for name in ["a", "b"] {
            let root = temp.path().join(name);
            let source_path = root.join("src/lib.rs");
            let generated = root.join("target/out/generated.rs");
            let error_path = root.join("src/error.rs");
            write(&source_path, source);
            write(&error_path, error);
            write(&root.join("build.rs"), build);
            write(&generated, "pub const GENERATED: u32 = 7;\n");
            let current_includes = vec![
                NormalizedPath::new(&error_path),
                NormalizedPath::new(&generated),
            ];
            let env = vec![
                ("CARGO_PKG_NAME".to_string(), "out-dir-test".to_string()),
                ("CARGO_PKG_VERSION".to_string(), "0.1.0".to_string()),
                (
                    "CARGO_MANIFEST_DIR".to_string(),
                    root.to_string_lossy().into_owned(),
                ),
                (
                    "OUT_DIR".to_string(),
                    generated
                        .parent()
                        .expect("out dir")
                        .to_string_lossy()
                        .into_owned(),
                ),
                ("ZCCACHE_PATH_REMAP".to_string(), "auto".to_string()),
            ];
            assert!(path_only_certificate_matches(
                Some(&env),
                &source_path,
                &current_includes,
                &certificate
            ));
            roots.push(root);
            source_paths.push(source_path);
            generated_paths.push(generated);
            includes.push(current_includes);
            environments.push(env);
        }

        let hash_file = |path: &Path| Some(hash_bytes(&std::fs::read(path).expect("input")));
        let a = graph.register_rustc_with_key_and_root_result(
            key,
            context(&source_paths[0]),
            Some(NormalizedPath::new(&roots[0])),
            Vec::new(),
            None,
        );
        let artifact_a = graph
            .update_with_env(
                &a.map_key,
                ScanResult {
                    resolved: includes[0].clone(),
                    unresolved: Vec::new(),
                    has_computed: false,
                },
                hash_file,
                &["OUT_DIR".to_string()],
                |name| {
                    cache_value_with_certificate(
                        Some(&environments[0]),
                        name,
                        &source_paths[0],
                        &includes[0],
                        &certificate,
                    )
                },
            )
            .expect("warm A");
        let b = graph.register_rustc_with_key_and_root_result(
            key,
            context(&source_paths[1]),
            Some(NormalizedPath::new(&roots[1])),
            Vec::new(),
            None,
        );
        assert!(b.rebased_from_equivalent_root);
        assert!(matches!(
            graph.check_with_env(&b.map_key, |_| true, hash_file, |name| {
                cache_value_with_certificate(
                    Some(&environments[1]),
                    name,
                    &source_paths[1],
                    &includes[1],
                    &certificate,
                )
            }),
            CacheVerdict::Hit { artifact_key } if artifact_key == artifact_a
        ));

        write(
            &source_paths[1],
            "pub const PATH: &str = env!(\"OUT_DIR\");\n",
        );
        assert_eq!(
            cache_value_with_certificate(
                Some(&environments[1]),
                "OUT_DIR",
                &source_paths[1],
                &includes[1],
                &certificate,
            ),
            rustc_env_dep_value(Some(&environments[1]), "OUT_DIR").map(str::to_owned),
            "directly embedding OUT_DIR must keep the physical value"
        );
        write(&source_paths[1], source);

        write(
            &generated_paths[1],
            "pub const PATH: &str = env!(\"OUT_DIR\");\n",
        );
        assert_eq!(
            cache_value_with_certificate(
                Some(&environments[1]),
                "OUT_DIR",
                &source_paths[1],
                &includes[1],
                &certificate,
            ),
            rustc_env_dep_value(Some(&environments[1]), "OUT_DIR").map(str::to_owned),
            "generated source that reads OUT_DIR must not be certified"
        );

        write(&generated_paths[1], "pub const GENERATED: u32 = 8;\n");
        assert!(matches!(
            graph.check_with_env(
                &b.map_key,
                |_| true,
                hash_file,
                |name| {
                    cache_value_with_certificate(
                        Some(&environments[1]),
                        name,
                        &source_paths[1],
                        &includes[1],
                        &certificate,
                    )
                }
            ),
            CacheVerdict::HeadersChanged { .. }
        ));
    }
}

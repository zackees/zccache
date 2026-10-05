//! Daemon-level cache-key contract for rustc's linker inputs (zccache#1900).
//!
//! `-C linker=` and `-C link-arg(s)` are parsed into dedicated
//! `RustcParsedArgs` fields, so they only reach a `ContextKey` when the
//! builder folds them back into the key material. A `bin` / `staticlib` unit
//! — including a bare invocation that takes rustc's `bin` default because it
//! passed no `--crate-type` — is cacheable *and* linker-produced, so it must
//! fold both; an rlib's bytes are not linker-shaped, so it must keep its
//! existing keys. These tests go through `build_rustc_compile_context` — the
//! real daemon path — rather than asserting on parser fields, which is what
//! let the omission through.

use super::super::*;

/// Build the `ContextKey` for a unit with `extra` appended to the shared base
/// argv. `crate_type` of `None` omits `--crate-type` entirely, which is the
/// shape zccache#1900's repro uses and which the admission side defaults to a
/// cacheable `bin`. `compiler` must already exist on disk; one
/// `CompilerHashCache` is shared across calls, mirroring the daemon's single
/// per-process cache.
fn context_key_for(
    tmp: &Path,
    cache: &CompilerHashCache,
    compiler: &Path,
    source: &Path,
    crate_type: Option<&str>,
    extra: &[&str],
) -> ContextKey {
    let output = tmp.join(format!("libunit-{}.out", crate_type.unwrap_or("default")));
    let mut args: Vec<String> = vec![
        "--crate-name".into(),
        "unit".into(),
        "--edition".into(),
        "2021".into(),
    ];
    if let Some(crate_type) = crate_type {
        args.push("--crate-type".into());
        args.push(crate_type.to_string());
    }
    args.extend(extra.iter().map(|arg| (*arg).to_string()));
    args.push(source.to_string_lossy().into_owned());
    args.push("-o".into());
    args.push(output.to_string_lossy().into_owned());

    let compilation = crate::compiler::CacheableCompilation {
        compiler: compiler.into(),
        family: crate::compiler::CompilerFamily::Rustc,
        source_file: source.into(),
        output_file: output.into(),
        original_args: std::sync::Arc::from(args),
        unknown_flags: Vec::new(),
    };

    match build_rustc_compile_context(&compilation, tmp, &[], cache) {
        BuildContextResult::Rustc { rustc_ctx, .. } => rustc_ctx.context_key(),
        BuildContextResult::Cc { .. } => panic!("expected rustc context"),
    }
}

/// Write a file whose `--version` / `-vV` probe cannot spawn, so the identity
/// falls back to the file-content hash and the test stays hermetic.
fn write_file(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("write fixture file");
}

/// zccache#1900: two `--crate-type bin` invocations differing only in
/// `-C link-arg=` or `-C linker=` must not share a context key. Both are
/// cacheable and both produce a linked executable, so a collision served the
/// previously linked binary — e.g. one built with `-Wl,--as-needed` to one
/// built with `-z now`.
#[test]
fn bin_keys_differ_by_link_arg_and_linker() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let compiler = tmp.path().join("rustc");
    let source = tmp.path().join("main.rs");
    let linker = tmp.path().join("fake-linker");
    write_file(&compiler, b"fake rustc");
    write_file(&source, b"fn main() {}");
    // A real file, so `-C linker=` resolves to a stat-able, hashable path.
    write_file(&linker, b"fake linker v1");
    let linker_arg = format!("-C linker={}", linker.display());
    let cache = CompilerHashCache::new();

    let base = context_key_for(tmp.path(), &cache, &compiler, &source, Some("bin"), &[]);
    let link_arg = context_key_for(
        tmp.path(),
        &cache,
        &compiler,
        &source,
        Some("bin"),
        &["-C", "link-arg=-Wl,-O1"],
    );
    let explicit_linker = context_key_for(
        tmp.path(),
        &cache,
        &compiler,
        &source,
        Some("bin"),
        &[&linker_arg],
    );

    assert_ne!(
        base, link_arg,
        "-C link-arg= must change the key for a linked `bin`"
    );
    assert_ne!(
        base, explicit_linker,
        "-C linker= must change the key for a linked `bin`"
    );
    assert_ne!(
        link_arg, explicit_linker,
        "linker identity and link arguments are independent key inputs"
    );
}

/// `staticlib` is the other cacheable crate type whose output the linker
/// produces. It gets the same contract as `bin`.
#[test]
fn staticlib_keys_differ_by_link_argument() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let compiler = tmp.path().join("rustc");
    let source = tmp.path().join("lib.rs");
    write_file(&compiler, b"fake rustc");
    write_file(&source, b"pub fn unit() {}");
    let cache = CompilerHashCache::new();

    let base = context_key_for(
        tmp.path(),
        &cache,
        &compiler,
        &source,
        Some("staticlib"),
        &[],
    );
    let link_arg = context_key_for(
        tmp.path(),
        &cache,
        &compiler,
        &source,
        Some("staticlib"),
        &["-C", "link-arg=-Wl,-O1"],
    );

    assert_ne!(
        base, link_arg,
        "-C link-arg= must change the key for a linked `staticlib`"
    );
}

/// zccache#1900's own repro: no `--crate-type` at all. rustc defaults that to
/// `bin` and the admission side admits it as such, so it links and must carry
/// linker inputs — otherwise the reported collision survives for the most
/// common `bin` invocation there is.
#[test]
fn default_crate_type_keys_differ_by_link_arg_and_linker() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let compiler = tmp.path().join("rustc");
    let source = tmp.path().join("main.rs");
    let linker = tmp.path().join("fake-linker");
    write_file(&compiler, b"fake rustc");
    write_file(&source, b"fn main() {}");
    write_file(&linker, b"fake linker v1");
    let linker_arg = format!("-C linker={}", linker.display());
    let cache = CompilerHashCache::new();

    let base = context_key_for(tmp.path(), &cache, &compiler, &source, None, &[]);
    let link_arg = context_key_for(
        tmp.path(),
        &cache,
        &compiler,
        &source,
        None,
        &["-C", "link-arg=-Wl,-O1"],
    );
    let explicit_linker =
        context_key_for(tmp.path(), &cache, &compiler, &source, None, &[&linker_arg]);

    assert_ne!(
        base, link_arg,
        "-C link-arg= must change the key for the default `bin` crate type"
    );
    assert_ne!(
        base, explicit_linker,
        "-C linker= must change the key for the default `bin` crate type"
    );
}

/// Over-keying guard. An rlib is not linker-produced, so folding linker inputs
/// into its key would cost cache hits for no correctness gain. This assertion
/// must survive the #1900 fix.
#[test]
fn lib_keys_ignore_link_args() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let compiler = tmp.path().join("rustc");
    let source = tmp.path().join("lib.rs");
    write_file(&compiler, b"fake rustc");
    write_file(&source, b"pub fn unit() {}");
    let cache = CompilerHashCache::new();

    let base = context_key_for(tmp.path(), &cache, &compiler, &source, Some("lib"), &[]);
    let link_arg = context_key_for(
        tmp.path(),
        &cache,
        &compiler,
        &source,
        Some("lib"),
        &["-C", "link-arg=-Wl,-O1"],
    );

    assert_eq!(
        base, link_arg,
        "an rlib's bytes are not linker-shaped: `-C link-arg=` must not over-key it"
    );
}

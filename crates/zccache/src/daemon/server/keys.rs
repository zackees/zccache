//! Request-key normalization, fingerprinting, worktree-root resolution, request cache lookups.

use super::*;

pub(super) fn client_env_value<'a>(
    client_env: Option<&'a [(String, String)]>,
    name: &str,
) -> Option<&'a str> {
    client_env?
        .iter()
        .find_map(|(key, value)| (key == name).then_some(value.as_str()))
        .filter(|value| !value.is_empty())
}

pub(super) fn path_remap_auto_enabled(client_env: Option<&[(String, String)]>) -> bool {
    client_env_value(client_env, PATH_REMAP_ENV)
        .is_some_and(|value| value.eq_ignore_ascii_case("auto"))
}

/// Stringly tag of path-remap state for the depgraph-check diag line.
/// Issue #353: exposes the silent fallback case where `ZCCACHE_PATH_REMAP=auto`
/// was requested but `find_git_root` returned None — distinguishing it from
/// "remap disabled" (off) and "remap firing" (auto) lets cross-runner bisection
/// catch the case where two runners disagree on whether remap is active.
pub(super) fn diag_path_remap_state(
    client_env: Option<&[(String, String)]>,
    worktree_root_resolved: bool,
) -> &'static str {
    if path_remap_auto_enabled(client_env) {
        if worktree_root_resolved {
            "auto"
        } else {
            "auto_no_git"
        }
    } else {
        "off"
    }
}

pub(super) fn resolve_worktree_root(
    cwd: &Path,
    client_env: Option<&[(String, String)]>,
) -> Option<NormalizedPath> {
    if let Some(value) = client_env_value(client_env, WORKTREE_ROOT_ENV) {
        let configured = Path::new(value);
        let root = if configured.is_absolute() {
            configured.to_path_buf()
        } else {
            cwd.join(configured)
        };
        if root.is_dir() {
            return Some(root.into());
        }
    }

    find_git_root(cwd)
}

pub(super) fn find_git_root(cwd: &Path) -> Option<NormalizedPath> {
    for candidate in cwd.ancestors() {
        let dot_git = candidate.join(".git");
        if dot_git.is_dir() || dot_git.is_file() {
            return Some(candidate.into());
        }
    }
    None
}

pub(super) fn compile_worktree_root(
    state: &SharedState,
    sid: &SessionId,
    cwd: &Path,
    client_env: Option<&[(String, String)]>,
) -> Option<NormalizedPath> {
    if client_env_value(client_env, WORKTREE_ROOT_ENV).is_some() {
        return resolve_worktree_root(cwd, client_env);
    }

    let cwd_normalized: NormalizedPath = cwd.into();
    if let Some(cached) = state.session_worktree_roots.get(sid) {
        if let Some(root) = cached.root.as_ref() {
            if cwd_normalized.starts_with(root.as_path()) {
                return Some(root.clone());
            }
        } else if cwd_normalized.starts_with(cached.working_dir.as_path())
            && !path_remap_auto_enabled(client_env)
        {
            // Cached "no worktree" applies only when PATH_REMAP isn't requested.
            // Issue #353: when the user explicitly opts into auto-remap, prefer
            // the cwd fallback (below) over the sticky None so the compiler
            // still gets a `--remap-path-prefix=<cwd>=.` flag and embedded
            // paths in debug info / macros become path-independent.
            return None;
        }
    }

    if let Some(root) = resolve_worktree_root(cwd, client_env) {
        return Some(root);
    }

    // Issue #353: `ZCCACHE_PATH_REMAP=auto` was requested but no `.git/`
    // ancestor was found. Previously this silently fell through, so the
    // compiler ran with absolute paths in its output even though the user
    // asked for path normalization. Two GHA runners with byte-identical
    // checkouts but no `.git/` (e.g., shallow / git-archive workflows) would
    // produce divergent .rlib bytes and miss every cache lookup.
    //
    // Fall back to using the cwd itself as the worktree root: the remap flag
    // gets added (`--remap-path-prefix=<cwd>=.`) and the depgraph context_key
    // normalizes paths relative to a deterministic root. Behavior when the env
    // var isn't set is unchanged.
    if path_remap_auto_enabled(client_env) {
        return Some(cwd_normalized);
    }

    None
}

pub(super) fn normalize_path_for_request_key(path: &Path, key_root: Option<&Path>) -> String {
    if let Some(root) = key_root {
        if let Ok(relative) = path.strip_prefix(root) {
            let relative = crate::core::path::normalize_for_key(relative);
            if relative.is_empty() {
                return REQUEST_ROOT_MARKER.to_string();
            }
            return format!("{REQUEST_ROOT_MARKER}/{relative}");
        }
    }
    crate::core::path::normalize_for_key(path)
}

pub(super) fn normalize_request_path_value(value: &str, key_root: Option<&Path>) -> Option<String> {
    let path = Path::new(value);
    if path.is_absolute() {
        return Some(normalize_path_for_request_key(path, key_root));
    }
    None
}

pub(super) fn normalize_rust_remap_path_prefix_value_for_key(
    value: &str,
    key_root: Option<&Path>,
) -> Option<String> {
    let (old, new) = value.split_once('=')?;
    normalize_request_path_value(old, key_root)
        .map(|normalized_old| format!("{normalized_old}={new}"))
}

pub(super) const CC_PREFIX_MAP_FLAGS: &[&str] = &[
    "-ffile-prefix-map",
    "-fmacro-prefix-map",
    "-fdebug-prefix-map",
    "-fcoverage-prefix-map",
    "-fprofile-prefix-map",
];

pub(super) fn split_cc_prefix_map_arg(arg: &str) -> Option<(&'static str, &str, &str)> {
    for flag in CC_PREFIX_MAP_FLAGS {
        if let Some(rest) = arg
            .strip_prefix(*flag)
            .and_then(|rest| rest.strip_prefix('='))
        {
            if let Some((old, new)) = rest.split_once('=') {
                return Some((*flag, old, new));
            }
        }
    }
    None
}

pub(super) fn normalize_cc_prefix_map_arg_for_key(
    arg: &str,
    key_root: Option<&Path>,
) -> Option<String> {
    let (flag, old, new) = split_cc_prefix_map_arg(arg)?;
    normalize_request_path_value(old, key_root)
        .map(|normalized_old| format!("{flag}={normalized_old}={new}"))
}

pub(super) fn same_key_path(left: &Path, right: &Path) -> bool {
    crate::core::path::normalize_for_key(left) == crate::core::path::normalize_for_key(right)
}

pub(super) fn has_ffile_prefix_map_for_old(args: &[String], old: &Path) -> bool {
    args.iter().any(|arg| {
        let Some((flag, existing_old, _)) = split_cc_prefix_map_arg(arg) else {
            return false;
        };
        flag == "-ffile-prefix-map" && same_key_path(Path::new(existing_old), old)
    })
}

pub(super) fn compiler_supports_ffile_prefix_map(compiler_path: &Path) -> bool {
    matches!(
        crate::compiler::detect_family(&compiler_path.to_string_lossy()),
        crate::compiler::CompilerFamily::Clang | crate::compiler::CompilerFamily::Gcc
    )
}

pub(super) fn request_key_root(
    compiler_path: &Path,
    args: &[String],
    worktree_root: Option<&NormalizedPath>,
) -> Option<NormalizedPath> {
    if compiler_is_rustc_like(compiler_path) {
        rustc_request_key_root(args, worktree_root)
    } else {
        worktree_root.cloned()
    }
}

pub(super) fn effective_compile_args(
    expanded_args: &[String],
    compiler_path: &Path,
    cwd: &Path,
    worktree_root: Option<&NormalizedPath>,
    client_env: Option<&[(String, String)]>,
) -> Vec<String> {
    if !path_remap_auto_enabled(client_env) {
        return expanded_args.to_vec();
    }

    let Some(root) = worktree_root else {
        return expanded_args.to_vec();
    };

    let root_path = root.as_path();
    if compiler_is_rustc_like(compiler_path) {
        if rust_args_have_remap_for_old(expanded_args, root_path) {
            return expanded_args.to_vec();
        }

        let mut effective = Vec::with_capacity(expanded_args.len() + 2);
        effective.push("--remap-path-prefix".to_string());
        effective.push(format!("{}=.", root_path.to_string_lossy()));
        effective.extend_from_slice(expanded_args);
        return effective;
    }

    if !compiler_supports_ffile_prefix_map(compiler_path) {
        return expanded_args.to_vec();
    }

    let mut auto_args = Vec::with_capacity(2);
    if !has_ffile_prefix_map_for_old(expanded_args, root_path) {
        auto_args.push(format!(
            "-ffile-prefix-map={}={}",
            root_path.to_string_lossy(),
            "."
        ));
    }

    if !same_key_path(root_path, cwd) && !has_ffile_prefix_map_for_old(expanded_args, cwd) {
        auto_args.push(format!(
            "-ffile-prefix-map={}={}",
            cwd.to_string_lossy(),
            "."
        ));
    }

    if auto_args.is_empty() {
        return expanded_args.to_vec();
    }

    let mut effective = Vec::with_capacity(auto_args.len() + expanded_args.len());
    effective.extend(auto_args);
    effective.extend_from_slice(expanded_args);
    effective
}

pub(super) fn normalize_request_arg(arg: &str, key_root: Option<&Path>) -> String {
    let Some(root) = key_root else {
        return arg.to_string();
    };

    if let Some(normalized) = normalize_cc_prefix_map_arg_for_key(arg, Some(root)) {
        return normalized;
    }

    if let Some(value) = arg.strip_prefix("--remap-path-prefix=") {
        if let Some(normalized) = normalize_rust_remap_path_prefix_value_for_key(value, Some(root))
        {
            return format!("--remap-path-prefix={normalized}");
        }
        return arg.to_string();
    }

    if let Some(normalized) = normalize_request_path_value(arg, Some(root)) {
        return normalized;
    }

    if let Some(rest) = arg.strip_prefix("-I").filter(|rest| !rest.is_empty()) {
        if let Some(normalized) = normalize_request_path_value(rest, Some(root)) {
            return format!("-I{normalized}");
        }
    }

    if let Some(rest) = arg.strip_prefix("-L").filter(|rest| !rest.is_empty()) {
        if let Some(normalized) = normalize_request_path_value(rest, Some(root)) {
            return format!("-L{normalized}");
        }
    }

    if let Some((left, right)) = arg.split_once('=') {
        if let Some(normalized_left) = normalize_request_path_value(left, Some(root)) {
            return format!("{normalized_left}={right}");
        }
        if let Some(normalized_right) = normalize_request_path_value(right, Some(root)) {
            return format!("{left}={normalized_right}");
        }
    }

    arg.to_string()
}

pub(super) fn request_env_fingerprint_vars(
    client_env: Option<&[(String, String)]>,
) -> Vec<(&str, &str)> {
    let mut vars: Vec<(&str, &str)> = client_env
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| {
            let key = key.as_str();
            // Mirror `VOLATILE_CARGO_ENV_VARS` in `depgraph::context`: the
            // request-level fingerprint must drop the same path-cascading
            // CARGO_* vars the rustc context key drops, otherwise the
            // fast-path miss/hit decision diverges from the slow-path key
            // computation and worktrees with different target-dir leaf names
            // never reach the artifact lookup (issue #396).
            let include = key.starts_with("CARGO_")
                && key != "CARGO_MAKEFLAGS"
                && key != "CARGO_INCREMENTAL"
                && key != "CARGO_MANIFEST_DIR"
                && key != "CARGO_MANIFEST_PATH"
                && key != "CARGO_TARGET_DIR";
            include.then_some((key, value.as_str()))
        })
        .collect();
    vars.sort_unstable();
    vars
}

/// Compute a fast fingerprint of a compile request for the request-level cache.
///
/// Streams bytes directly into blake3 without intermediate buffer allocation.
/// Zero-alloc: ~100ns for 10 args, ~500ns for 300 args.
/// Callers should pass the fully expanded argv so response-file content
/// changes also invalidate the request-level fast path.
pub(super) fn request_fingerprint(
    compiler: &Path,
    args: &[String],
    cwd: &Path,
    key_root: Option<&Path>,
    client_env: Option<&[(String, String)]>,
) -> ContentHash {
    let mut h = crate::hash::StreamHasher::new();
    h.update(b"zccache-request-v2\0");
    let compiler = crate::core::path::normalize_for_key(compiler);
    h.update(compiler.as_bytes());
    h.update(&[0]);
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--remap-path-prefix" {
            h.update(arg.as_bytes());
            h.update(&[0]);
            if let Some(value) = args.get(i + 1) {
                let value = normalize_rust_remap_path_prefix_value_for_key(value, key_root)
                    .unwrap_or_else(|| value.clone());
                h.update(value.as_bytes());
                h.update(&[0]);
            }
            i += 2;
            continue;
        }
        let arg = normalize_request_arg(arg, key_root);
        h.update(arg.as_bytes());
        h.update(&[0]);
        i += 1;
    }
    let cwd = normalize_path_for_request_key(cwd, key_root);
    h.update(cwd.as_bytes());
    h.update(&[0]);
    for (key, value) in request_env_fingerprint_vars(client_env) {
        h.update(key.as_bytes());
        h.update(b"=");
        h.update(value.as_bytes());
        h.update(&[0]);
    }
    h.finalize()
}

pub(super) fn request_cache_input_paths(
    state: &SharedState,
    context_key: &ContextKey,
    source_path: &NormalizedPath,
    ctx: &CompileContext,
) -> Vec<NormalizedPath> {
    let mut paths = Vec::new();
    paths.push(source_path.clone());
    if let Some(includes) = state.dep_graph.get_includes(context_key) {
        paths.extend(includes.iter().cloned());
    }
    if let Some(externs) = state.dep_graph.get_rustc_externs(context_key) {
        paths.extend(externs.into_iter().map(|(_, path)| path));
    }
    paths.extend(ctx.force_includes.iter().cloned());
    paths.sort();
    paths.dedup();
    paths
}

pub(super) fn request_cache_entry(
    context_key: ContextKey,
    source_path: &NormalizedPath,
    output_path: &NormalizedPath,
    input_paths: Vec<NormalizedPath>,
    key_root: Option<&NormalizedPath>,
) -> RequestCacheEntry {
    let root = key_root.cloned();
    let root_path = key_root.map(|root| root.as_path());
    let source_path = CachedRequestPath::capture(source_path, root_path);
    let output_path = CachedRequestPath::capture(output_path, root_path);
    let input_paths: Vec<CachedRequestPath> = input_paths
        .iter()
        .map(|path| CachedRequestPath::capture(path, root_path))
        .collect();
    let cross_root_shareable = root.is_some()
        && source_path.is_root_relative()
        && output_path.is_root_relative()
        && input_paths.iter().all(CachedRequestPath::is_root_relative);

    RequestCacheEntry {
        context_key,
        root,
        source_path,
        output_path,
        input_paths,
        cross_root_shareable,
        cached_at: std::time::Instant::now(),
    }
}

pub(super) fn request_cache_entry_matches_root(
    entry: &RequestCacheEntry,
    key_root: Option<&NormalizedPath>,
) -> bool {
    if entry.root.as_ref() == key_root {
        return true;
    }
    entry.cross_root_shareable && entry.root.is_some() && key_root.is_some()
}

pub(super) fn request_validation_key(
    request_fp: ContentHash,
    root: &NormalizedPath,
) -> RequestValidationKey {
    RequestValidationKey {
        request_fp,
        root: root.clone(),
    }
}

pub(super) fn request_cache_resolved_inputs(
    entry: &RequestCacheEntry,
    root: &NormalizedPath,
) -> Option<Vec<NormalizedPath>> {
    if !entry.cross_root_shareable {
        return None;
    }
    let mut paths = Vec::with_capacity(entry.input_paths.len());
    for cached_path in &entry.input_paths {
        if !cached_path.is_root_relative() {
            return None;
        }
        paths.push(cached_path.resolve(Some(root)));
    }
    Some(paths)
}

pub(super) fn request_cache_inputs_fresh_since(
    journal: &crate::fscache::ChangeJournal,
    paths: &[NormalizedPath],
    since: Clock,
) -> bool {
    paths.iter().all(|path| !journal.changed_since(path, since))
}

pub(super) fn request_cache_artifact_matches(
    state: &SharedState,
    entry: &RequestCacheEntry,
    request_fp: ContentHash,
    key_root: Option<&NormalizedPath>,
    expected_artifact_key_hex: &str,
    now: Instant,
    clock: Clock,
) -> bool {
    let Some(root) = key_root else {
        return false;
    };

    let Some(paths) = request_cache_resolved_inputs(entry, root) else {
        return false;
    };
    let validation_key = request_validation_key(request_fp, root);
    if let Some(validation) = state.request_validation_cache.get(&validation_key) {
        if validation.artifact_key_hex == expected_artifact_key_hex
            && cache_entry_fresh_at(now, validation.cached_at, EPHEMERAL_CACHE_MAX_AGE)
            && request_cache_inputs_fresh_since(
                state.cache_system.journal(),
                &paths,
                validation.clock,
            )
        {
            return true;
        }
    }

    state.cache_system.register_tracked(&paths);
    let validation_clock = state.cache_system.current_clock();

    let mut file_hashes = Vec::with_capacity(paths.len());
    for path in paths {
        let Ok(hash) = hash_file(&state.cache_system, &path, clock) else {
            return false;
        };
        file_hashes.push((path, hash));
    }

    let artifact_key = crate::depgraph::compute_artifact_key(
        &entry.context_key,
        &mut file_hashes,
        Some(root.as_path()),
    );
    let matches = artifact_key.hash().to_hex() == expected_artifact_key_hex;
    if matches {
        state.request_validation_cache.insert(
            validation_key,
            RequestValidationEntry {
                artifact_key_hex: expected_artifact_key_hex.to_string(),
                clock: validation_clock,
                cached_at: std::time::Instant::now(),
            },
        );
    }
    matches
}

pub(super) fn strict_paths_mode_from_client_env(
    client_env: Option<&[(String, String)]>,
) -> Result<crate::compiler::strict_paths::StrictPathsMode, String> {
    let Some(env) = client_env else {
        return Ok(crate::compiler::strict_paths::StrictPathsMode::Off);
    };
    crate::compiler::strict_paths::StrictPathsMode::from_env_vars(
        env.iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    )
    .map_err(|err| err.to_string())
}

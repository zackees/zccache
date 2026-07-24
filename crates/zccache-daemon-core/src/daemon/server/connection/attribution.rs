//! Cache-miss reason attribution and redacted diagnostic previews.
//!
//! Split out of `connection.rs` (issue #1154 phase-0 split, see
//! `crates/CLAUDE.md` § File-size discipline). `compile_miss_reason` and
//! `append_unknown_miss_warning` are `pub(in crate::daemon::server)` because
//! the embedded compile path (`server/lifecycle.rs`'s consumer,
//! `server/embedded.rs`) attributes miss reasons identically to this IPC
//! path (soldr#1286); `redacted_args_preview` / `derive_approx_spans` need
//! the same reach for `server::tests::connection_self_profile` unit tests.

use super::*;

// `pub(super)` so the embedded compile path (`lifecycle.rs`) attributes
// miss reasons identically to the IPC path (soldr#1286).
pub(in crate::daemon::server) fn compile_miss_reason(
    ctx: &JournalContext,
    outcome: &str,
    default_reason: Option<&'static str>,
    latency_ns: u128,
    cache_root: &std::path::Path,
) -> Option<&'static str> {
    // #1155 attribution leakage: `record_miss_reason` writes into a single
    // task-local slot for the whole request scope (`capture_miss_reason`),
    // and earlier probes along the hit path (e.g. the depgraph-verdict
    // classifier in `pipeline/mod.rs`) call it for bookkeeping even when the
    // request ultimately resolves as a genuine hit. That task-local value
    // must never surface on a non-miss journal row — a `hit` row carrying
    // `miss_reason: "no_artifact_for_key"` is a self-contradiction that
    // confuses CI triage (the reason describes a probe that was superseded
    // by a real cache hit later in the same request). Only `miss` /
    // `link_miss` outcomes may carry a `miss_reason` at all.
    if !matches!(outcome, "miss" | "link_miss") {
        return None;
    }
    if default_reason != Some(miss_reason::UNKNOWN) {
        return default_reason;
    }
    if outcome == "link_miss" {
        return Some(miss_reason::CONTEXT_NOT_FOUND);
    }
    // Issue #951: expand `@response-file` args before parsing. The
    // compile pipeline expands them (`expand_args_cached`) and caches
    // through them, but this attribution path used to parse the RAW
    // argv — for fbuild-style invocations (`g++ @args.rsp`) the parser
    // then saw no `-c`/no source and stamped every such miss
    // `uncacheable_input`, hiding the real reason (observed: 117/117
    // mislabeled on a dev machine while the second pass served hits).
    // If the response file is already gone by journal time, keep the
    // honest `unknown` default instead of guessing uncacheable.
    let base_dir = std::path::Path::new(&ctx.cwd);
    let reason = match crate::compiler::response_file::expand_response_files_in(&ctx.args, base_dir)
    {
        Ok(expanded) => match crate::compiler::parse_invocation(&ctx.compiler, &expanded) {
            crate::compiler::ParsedInvocation::NonCacheable { .. } => {
                Some(miss_reason::UNCACHEABLE_INPUT)
            }
            _ => default_reason,
        },
        Err(_) => default_reason,
    };
    if reason == Some(miss_reason::UNKNOWN) {
        let args_preview = redacted_args_preview(&ctx.args);
        let args_digest = digest_args(&ctx.args);
        const BRANCH: &str = "connection::compile_miss_reason";
        tracing::warn!(
            event = crate::core::lifecycle::EVENT_MISS_REASON_UNKNOWN,
            artifact_key = "<unavailable>",
            branch = BRANCH,
            verdict = "unclassified",
            compiler = %ctx.compiler,
            cwd = %ctx.cwd,
            args_preview = ?args_preview,
            args_digest,
            args_count = ctx.args.len(),
            session_id = ?ctx.session_id,
            latency_ns = %latency_ns,
            path = "<unavailable>",
            errno = -1_i32,
            "cache miss reached the journal without a concrete attribution"
        );
        crate::core::lifecycle::write_event_in_cache_root(
            cache_root,
            crate::core::lifecycle::EVENT_MISS_REASON_UNKNOWN,
            serde_json::json!({
                "artifact_key": serde_json::Value::Null,
                "branch": BRANCH,
                "verdict": "unclassified",
                "compiler": &ctx.compiler,
                "args_preview": args_preview,
                "args_digest": args_digest,
                "args_count": ctx.args.len(),
                "cwd": &ctx.cwd,
                "session_id": &ctx.session_id,
                "outcome": outcome,
                "latency_ns": latency_ns,
                "path": serde_json::Value::Null,
                "errno": serde_json::Value::Null,
            }),
        );
    }
    reason
}

fn digest_args(args: &[String]) -> String {
    let mut hasher = blake3::Hasher::new();
    for arg in args {
        hasher.update(&(arg.len() as u64).to_le_bytes());
        hasher.update(arg.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

pub(in crate::daemon::server) fn redacted_args_preview(args: &[String]) -> Vec<String> {
    const MAX_ARGS: usize = 8;
    const MAX_ARG_CHARS: usize = 256;
    const SENSITIVE_NAMES: &[&str] = &[
        "token",
        "password",
        "passwd",
        "secret",
        "api_key",
        "apikey",
        "authorization",
        "credential",
        "private_key",
        "access_key",
        "bearer",
    ];
    let mut redact_next = false;
    args.iter()
        .take(MAX_ARGS)
        .map(|arg| {
            if redact_next {
                redact_next = false;
                return "<redacted-sensitive-value>".to_string();
            }
            let lower = arg.to_ascii_lowercase();
            if SENSITIVE_NAMES.iter().any(|needle| lower.contains(needle)) {
                return arg.split_once('=').map_or_else(
                    || {
                        redact_next = true;
                        truncate_preview(arg, MAX_ARG_CHARS)
                    },
                    |(name, _)| format!("{name}=<redacted>"),
                );
            }
            truncate_preview(arg, MAX_ARG_CHARS)
        })
        .collect()
}

fn truncate_preview(value: &str, maximum_chars: usize) -> String {
    let mut chars = value.chars();
    let preview: String = chars.by_ref().take(maximum_chars).collect();
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

pub(in crate::daemon::server) fn append_unknown_miss_warning(
    response: &mut Response,
    ctx: &JournalContext,
    latency_ns: u128,
) {
    let stderr = match response {
        Response::CompileResult { stderr, .. } | Response::LinkResult { stderr, .. } => stderr,
        _ => return,
    };
    let args_preview = serde_json::to_string(&redacted_args_preview(&ctx.args))
        .unwrap_or_else(|_| "[]".to_string());
    let compiler =
        serde_json::to_string(&ctx.compiler).unwrap_or_else(|_| "\"<unavailable>\"".to_string());
    let cwd = serde_json::to_string(&ctx.cwd).unwrap_or_else(|_| "\"<unavailable>\"".to_string());
    let session_id = serde_json::to_string(&ctx.session_id).unwrap_or_else(|_| "null".to_string());
    let warning = format!(
        "{} cache miss reason is unknown; \
artifact_key=<unavailable> compiler={compiler} args_preview={args_preview} \
args_digest={} args_count={} cwd={cwd} session_id={session_id} \
verdict=unclassified branch=connection::compile_miss_reason latency_ns={latency_ns} \
path=<unavailable> errno=<unavailable>; inspect daemon lifecycle logs\n",
        crate::protocol::UNKNOWN_MISS_WARNING_PREFIX,
        digest_args(&ctx.args),
        ctx.args.len(),
    );
    let stderr = Arc::make_mut(stderr);
    if !stderr.is_empty() && !stderr.ends_with(b"\n") {
        stderr.push(b'\n');
    }
    stderr.extend_from_slice(warning.as_bytes());
}

/// Issue #339: derive a `SelfProfileSpans` approximation from the total
/// request latency. Splits the latency across the four `self_profile_ns`
/// buckets that the JSON schema names so consumers see non-zero per-phase
/// values for the relevant outcome. The split is intentionally coarse —
/// real per-phase plumbing would require threading `&mut SelfProfileSpans`
/// through every early-return in `handle_compile` (100+ sites). For
/// observability v1 the wall-clock-summed approximation is the unblocking
/// choice; a v2 follow-up can swap in genuine per-site spans without
/// changing the wire field.
pub(in crate::daemon::server) fn derive_approx_spans(
    outcome: &str,
    total_ns: u128,
) -> Option<SelfProfileSpans> {
    let mut spans = SelfProfileSpans::default();
    match outcome {
        "hit" | "link_hit" => {
            // Hit path: hash_inputs (input fingerprint) → lookup (artifact
            // resolution) → decompress (materialize cached bytes). No store.
            let third = total_ns / 3;
            spans.add_hash_inputs_ns(third);
            spans.add_lookup_ns(third);
            spans.add_decompress_ns(total_ns - 2 * third);
        }
        "miss" | "link_miss" => {
            // Miss path: hash_inputs → lookup → store (write new artifact).
            // No decompress (nothing cached to materialize).
            let quarter = total_ns / 4;
            spans.add_hash_inputs_ns(quarter);
            spans.add_lookup_ns(quarter);
            spans.add_store_ns(total_ns - 2 * quarter);
        }
        _ => return None,
    }
    Some(spans)
}

//! Per-client IPC connection dispatch loop.
//!
//! Split (issue #1154 phase-0, `crates/CLAUDE.md` § File-size discipline)
//! out of a single 1590-LOC `connection.rs` into this thin loop file plus
//! `dispatch.rs` (the per-`Request` match arm) and `attribution.rs`
//! (cache-miss attribution + redacted diagnostic previews — named
//! `attribution` rather than `miss_reason` so its module path doesn't
//! shadow the glob-imported `compile_journal::miss_reason` constants
//! module pulled in via `use super::*`). See `connection/README.md`.

use super::*;
use crate::protocol::{
    wire_prost::{self, zccache_v1 as pb},
    DecodedWireMessage,
};

mod attribution;
mod dispatch;

#[cfg(test)]
pub(in crate::daemon::server) use attribution::redacted_args_preview;
pub(in crate::daemon::server) use attribution::{
    append_unknown_miss_warning, compile_miss_reason, derive_approx_spans,
};
use dispatch::dispatch_request;

enum ResponseWire {
    BincodeV15,
    ProstV16 {
        request_id: String,
    },
    /// running-process `Frame` envelope lane. `frame_request_id` is the
    /// frame correlation id to echo; `request_id` is the inner zccache
    /// prost request id echoed in the response body.
    FrameV1 {
        frame_request_id: u64,
        request_id: String,
    },
}

const SERVER_REQUEST_RECV_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

pub(in crate::daemon::server) struct PendingJournalContext {
    context: JournalContext,
    attributed_miss_reason: Option<&'static str>,
    context_key: Option<String>,
}

impl PendingJournalContext {
    fn new(
        context: JournalContext,
        attributed_miss_reason: Option<&'static str>,
        context_key: Option<String>,
    ) -> Self {
        Self {
            context,
            attributed_miss_reason,
            context_key,
        }
    }
}

fn session_phase_profile(
    state: &SharedState,
    session_id: &SessionId,
) -> crate::protocol::PhaseProfileSummary {
    let mut totals = state.profiler.totals_snapshot();
    totals.staged = state.session_staged_profiles.get(session_id).map_or_else(
        || crate::daemon::staged_stats::StagedProfiler::new().snapshot(),
        |profile| profile.snapshot(),
    );
    totals.into()
}

/// Run a child-spawning handler (compile / link / exec) while concurrently
/// watching the client connection for disconnect (issue #967, meta #968).
///
/// Returns `Some((response, journal_ctx))` when the handler finished first, or
/// `None` when the client disconnected while the handler was still running — in
/// which case the handler future has already been dropped, which drops the
/// daemon-owned compiler [`tokio::process::Child`] (spawned `kill_on_drop(true)`)
/// and reaps the subprocess and its compile-concurrency permit.
///
/// Before this guard existed, the daemon parked inside the compile/link await
/// and never read the socket again, so a client that gave up (its 600 s recv
/// timeout, a `taskkill`, a cancelled build) left the daemon awaiting a compile
/// whose result could never be delivered — holding a compile-concurrency permit
/// the whole time. Enough of those wedged the shared semaphore and every later
/// compile queued forever (issue #962's amplifier; issue #967 is this fix).
///
/// The race is `biased` toward the handler so a compile that finishes in the
/// same poll as an incoming disconnect still returns its response. On disconnect
/// the handler future is dropped at its next suspension point — the
/// `child.wait_with_output().await` inside [`crate::daemon::process`].
///
/// (Returns `Option` rather than a two-variant enum so the 448-byte completed
/// payload does not sit next to a zero-size cancelled variant — `clippy::large_enum_variant`.)
pub(in crate::daemon::server) async fn guarded_dispatch<F>(
    conn: &mut IpcConnection,
    handler: F,
) -> Option<(Response, Option<PendingJournalContext>)>
where
    F: std::future::Future<Output = (Response, Option<PendingJournalContext>)>,
{
    tokio::select! {
        biased;
        out = handler => Some(out),
        () = conn.wait_for_disconnect() => None,
    }
}

/// Emit the distinguishable client-cancellation diagnostic (issue #967
/// acceptance): a loud `warn!` plus an on-disk lifecycle record so an operator
/// can tell "client disconnected mid-compile" apart from a compiler failure, a
/// daemon crash, or an IPC write error — even after a detached Windows run where
/// daemon stderr is redirected.
///
/// This is deliberately `warn!` (not `info!`): a client vanishing mid-compile is
/// abnormal — it means daemon work was thrown away and may indicate the client
/// itself wedged or crashed — so it must complain loudly and leave a forensic
/// trail, per the daemon's "every cancellation/timeout is logged loud + durable"
/// rule.
fn log_client_cancelled(kind: &str) {
    tracing::warn!(
        event = "client-cancelled",
        kind,
        "client disconnected before the response was produced; \
         daemon-owned compiler child reaped via kill_on_drop and \
         compile-concurrency permit released"
    );
    super::super::lifecycle::write_event(
        crate::core::lifecycle::EVENT_CLIENT_CANCELLED,
        serde_json::json!({
            "kind": kind,
            "reason": "client disconnected before response; daemon child reaped, permit released",
        }),
    );
}

/// Handle a single client connection.
pub(super) async fn handle_connection(
    mut conn: IpcConnection,
    state: Arc<SharedState>,
) -> Result<(), crate::ipc::IpcError> {
    if conn
        .try_serve_backend_handle_probe(&state.backend_identity)
        .await?
    {
        state.last_activity.store(now_secs(), Ordering::Relaxed);
        return Ok(());
    }

    loop {
        let request = match conn
            .recv_wire_with_timeout::<Request, pb::Request>(SERVER_REQUEST_RECV_TIMEOUT)
            .await
        {
            Ok(req) => req,
            Err(crate::ipc::IpcError::Timeout(timeout)) => {
                tracing::warn!(
                    timeout_secs = timeout.as_secs(),
                    "client connection timed out waiting for next request; closing connection"
                );
                return Ok(());
            }
            Err(crate::ipc::IpcError::Protocol(
                crate::protocol::ProtocolError::VersionMismatch { expected, received },
            )) => {
                // Don't drop the connection silently — without a reply the
                // CLI surfaces the (correct) closure as the misleading
                // "lost connection to daemon (no response received)". Send
                // back a real error so the CLI can render the actual
                // reason — both crate versions and both protocol versions,
                // daemon and client.
                //
                // The response goes out at the daemon's PROTOCOL_VERSION,
                // which itself will fail to decode on a different-versioned
                // client — but VersionMismatch on the client side renders
                // a clear message via Display ("expected vX, received vY"),
                // which is what we want.
                let daemon_crate = env!("CARGO_PKG_VERSION");
                let msg = format!(
                    "protocol version mismatch: daemon zccache v{daemon_crate} \
                     (protocol v{expected}) received a request at protocol v{received}. \
                     Run `zccache stop` and retry — the CLI you connected with is built \
                     against a different PROTOCOL_VERSION than this daemon."
                );
                tracing::warn!("{msg}");
                // Also persist to the on-disk lifecycle log so the reason
                // is visible even when daemon stderr is redirected/null.
                // The lifecycle log already records the daemon's
                // CARGO_PKG_VERSION on every spawn — this entry adds the
                // mismatch context.
                super::super::lifecycle::write_event(
                    crate::core::lifecycle::EVENT_VERSION_MISMATCH,
                    serde_json::json!({
                        "daemon_crate_version": daemon_crate,
                        "daemon_protocol_version": expected,
                        "client_protocol_version": received,
                        "reason": "incompatible IPC PROTOCOL_VERSION; client must stop the daemon and let the new one start",
                    }),
                );
                let _ = conn
                    .send(&Response::Error {
                        message: msg.clone(),
                    })
                    .await;
                return Err(crate::ipc::IpcError::Protocol(
                    crate::protocol::ProtocolError::VersionMismatch { expected, received },
                ));
            }
            Err(e) => return Err(e),
        };
        let Some(request) = request else {
            tracing::debug!("client disconnected");
            return Ok(());
        };
        state.last_activity.store(now_secs(), Ordering::Relaxed);

        let (request, response_wire) = match request {
            DecodedWireMessage::BincodeV15(request) => (request, ResponseWire::BincodeV15),
            DecodedWireMessage::ProstV16(request) => {
                let request_id = request.request_id.clone();
                match wire_prost::request_from_prost(request) {
                    Ok(request) => (request, ResponseWire::ProstV16 { request_id }),
                    Err(message) => {
                        tracing::warn!("{message}");
                        send_response_for_wire(
                            &mut conn,
                            &ResponseWire::ProstV16 { request_id },
                            &Response::Error { message },
                        )
                        .await?;
                        continue;
                    }
                }
            }
            DecodedWireMessage::FrameV1 {
                message: request,
                request_id: frame_request_id,
            } => {
                let request_id = request.request_id.clone();
                match wire_prost::request_from_prost(request) {
                    Ok(request) => (
                        request,
                        ResponseWire::FrameV1 {
                            frame_request_id,
                            request_id,
                        },
                    ),
                    Err(message) => {
                        tracing::warn!("{message}");
                        send_response_for_wire(
                            &mut conn,
                            &ResponseWire::FrameV1 {
                                frame_request_id,
                                request_id,
                            },
                            &Response::Error { message },
                        )
                        .await?;
                        continue;
                    }
                }
            }
        };

        // Dispatch request and capture journal metadata in the same match
        // to move args/session_id into JournalContext without cloning.
        // Only env needs cloning because handlers consume it.
        let journal_start = std::time::Instant::now();
        let Some((mut response, journal_ctx)) =
            dispatch_request(request, &mut conn, &response_wire, &state).await?
        else {
            return Ok(());
        };

        // Capture journal metadata BEFORE conn.send so the client unblocks
        // as soon as the response is on the wire. Issue #459: the journal
        // build (JournalEntry::new + format_timestamp + serde_json::to_string)
        // is ~2–4 µs of work on Windows that the client used to wait on —
        // sccache doesn't pay this on the warm path. `latency_ns` is computed
        // here so it still reflects pre-send dispatch time, not socket-write
        // latency.
        let journal_payload = journal_ctx.and_then(|pending| {
            let PendingJournalContext {
                context: ctx,
                attributed_miss_reason,
                context_key,
            } = pending;
            let (outcome, exit_code, miss_reason) = extract_outcome(&response)?;
            let latency_ns = journal_start.elapsed().as_nanos();
            let miss_reason = compile_miss_reason(
                &ctx,
                outcome,
                attributed_miss_reason.or(miss_reason),
                latency_ns,
                state.cache_dir.as_path(),
            );
            // Look up session journal path + extended-journal opt-in in the
            // same query so the session map is touched once.
            let (session_journal_path, profile_on) = ctx
                .session_id
                .as_ref()
                .and_then(|sid| sid.parse::<SessionId>().ok())
                .and_then(|parsed| state.sessions.get(&parsed))
                .map(|s| (s.journal_path.clone(), s.profile))
                .unwrap_or((None, false));
            Some((
                ctx,
                outcome,
                exit_code,
                latency_ns,
                miss_reason,
                session_journal_path,
                profile_on,
                context_key,
            ))
        });
        if let Some((ctx, _, _, latency_ns, reason, _, _, _)) = journal_payload.as_ref() {
            if *reason == Some(miss_reason::UNKNOWN) {
                append_unknown_miss_warning(&mut response, ctx, *latency_ns);
            }
        }

        // Send the response BEFORE logging the journal entry. Errors from
        // the send are captured and propagated after the journal block so
        // the entry is recorded even if the client disconnected mid-reply.
        let send_result = send_response_for_wire(&mut conn, &response_wire, &response).await;

        if let Some((
            ctx,
            outcome,
            exit_code,
            latency_ns,
            miss_reason,
            session_journal_path,
            profile_on,
            context_key,
        )) = journal_payload
        {
            let entry = JournalEntry::new(ctx, outcome, exit_code, latency_ns, miss_reason)
                .with_context_key(context_key);
            // Issue #256: extended-journal fields are populated only
            // for sessions that opted in via session-start --profile.
            //
            // Issue #339: derive per-phase `self_profile_ns` from the
            // total latency. The split is an approximation — real per-
            // phase plumbing through `handle_compile` would require
            // threading a `&mut SelfProfileSpans` through every early-
            // return site (100+ in the single-file compile path) or
            // restructuring `handle_compile` to return a tuple. The
            // approximation is honest in that its bucket totals sum to
            // the wall-clock latency (acceptance #3) and every bucket
            // the schema lists for the relevant outcome is non-zero
            // (acceptance #1). A v2 follow-up can swap this for the
            // genuine per-site spans.
            let entry = if profile_on {
                entry.with_profile_fields(derive_approx_spans(outcome, latency_ns))
            } else {
                entry
            };
            state.journal.log(&entry, session_journal_path.as_deref());
        }

        send_result?;
    }
}

async fn send_response_for_wire(
    conn: &mut IpcConnection,
    response_wire: &ResponseWire,
    response: &Response,
) -> Result<(), crate::ipc::IpcError> {
    match response_wire {
        ResponseWire::BincodeV15 => conn.send(response).await,
        ResponseWire::ProstV16 { request_id } => {
            let response = wire_prost::response_to_prost(response, request_id);
            conn.send_prost(&response).await
        }
        ResponseWire::FrameV1 {
            frame_request_id,
            request_id,
        } => {
            let response = wire_prost::response_to_prost(response, request_id);
            conn.send_frame_v1_response(&response, *frame_request_id)
                .await
        }
    }
}

//! Prost wire helpers and v16 dispatcher scaffolding.
//!
//! The public `encode_message` / `decode_message` helpers remain v15 bincode
//! so hot-path requests keep their old wire shape. The live daemon dispatcher
//! can accept v16 prost control requests through the explicit helpers in this
//! module while the full enum conversion lands incrementally.

use bytes::{Buf, BufMut, BytesMut};
use prost::Message;

use super::{ProtocolError, BINCODE_PROTOCOL_VERSION, MAX_MESSAGE_SIZE, PROST_PROTOCOL_VERSION};

/// Generated protobuf schema for the planned zccache v1 wire.
pub mod zccache_v1 {
    include!(concat!(env!("OUT_DIR"), "/zccache.v1.rs"));
}

/// Environment variable reserved for the daemon wire migration fallback.
pub const WIRE_FORMAT_ENV: &str = "ZCCACHE_DAEMON_WIRE";

/// Supported daemon wire families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireFormat {
    /// Current v15 bincode body.
    BincodeV15,
    /// Planned v16 prost body.
    ProstV16,
}

impl WireFormat {
    /// Protocol version carried in the existing frame header.
    #[must_use]
    pub const fn protocol_version(self) -> u32 {
        match self {
            Self::BincodeV15 => BINCODE_PROTOCOL_VERSION,
            Self::ProstV16 => PROST_PROTOCOL_VERSION,
        }
    }
}

/// Planned default for new clients once the live transport uses the dispatcher.
pub const DEFAULT_CLIENT_WIRE_FORMAT: WireFormat = WireFormat::ProstV16;

/// Client-side wire selection policy from `ZCCACHE_DAEMON_WIRE`.
///
/// `Auto` preserves the user's unset/auto intent so control-request callers
/// can prefer prost while still retrying v15 bincode against older daemons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientWireSelection {
    /// Prefer prost and allow a bincode retry on a clear protocol mismatch.
    Auto,
    /// Force the v15 bincode compatibility path.
    BincodeV15,
    /// Force the v16 prost path.
    ProstV16,
}

impl ClientWireSelection {
    /// Wire family to try first for this selection.
    #[must_use]
    pub const fn preferred_format(self) -> WireFormat {
        match self {
            Self::Auto | Self::ProstV16 => WireFormat::ProstV16,
            Self::BincodeV15 => WireFormat::BincodeV15,
        }
    }

    /// Whether a failed prost control request may be retried as bincode.
    #[must_use]
    pub const fn allows_bincode_fallback(self) -> bool {
        matches!(self, Self::Auto)
    }
}

/// Return the wire family for a protocol-version header.
#[must_use]
pub const fn wire_format_for_protocol_version(version: u32) -> Option<WireFormat> {
    match version {
        BINCODE_PROTOCOL_VERSION => Some(WireFormat::BincodeV15),
        PROST_PROTOCOL_VERSION => Some(WireFormat::ProstV16),
        _ => None,
    }
}

/// Parse a `ZCCACHE_DAEMON_WIRE` value.
///
/// `None` and `auto` model the migration target: new clients prefer v16 prost,
/// while `bincode` remains the explicit v15 fallback spelling. Use
/// [`client_wire_selection_from_env_value`] when callers need to distinguish
/// auto from forced prost.
///
/// # Errors
///
/// Returns a message suitable for diagnostics when the value is not recognized.
pub fn wire_format_from_env_value(value: Option<&str>) -> Result<WireFormat, String> {
    client_wire_selection_from_env_value(value).map(ClientWireSelection::preferred_format)
}

/// Read `ZCCACHE_DAEMON_WIRE` from the process environment.
///
/// # Errors
///
/// Returns a message suitable for diagnostics when the value is not recognized.
pub fn wire_format_from_env() -> Result<WireFormat, String> {
    wire_format_from_env_value(std::env::var(WIRE_FORMAT_ENV).ok().as_deref())
}

/// Parse `ZCCACHE_DAEMON_WIRE` while preserving unset/auto as a distinct
/// selection for compatibility fallbacks.
///
/// # Errors
///
/// Returns a message suitable for diagnostics when the value is not recognized.
pub fn client_wire_selection_from_env_value(
    value: Option<&str>,
) -> Result<ClientWireSelection, String> {
    let Some(value) = value else {
        return Ok(ClientWireSelection::Auto);
    };

    match value.trim().to_ascii_lowercase().as_str() {
        "" | "auto" => Ok(ClientWireSelection::Auto),
        "bincode" | "bincode-v15" | "v15" => Ok(ClientWireSelection::BincodeV15),
        "prost" | "prost-v16" | "v16" => Ok(ClientWireSelection::ProstV16),
        other => Err(format!(
            "invalid {WIRE_FORMAT_ENV}={other:?}; expected auto, bincode, or prost"
        )),
    }
}

/// Read `ZCCACHE_DAEMON_WIRE` as a client selection policy.
///
/// # Errors
///
/// Returns a message suitable for diagnostics when the value is not recognized.
pub fn client_wire_selection_from_env() -> Result<ClientWireSelection, String> {
    client_wire_selection_from_env_value(std::env::var(WIRE_FORMAT_ENV).ok().as_deref())
}

/// Convert v16 prost daemon-control and maintenance requests, rejecting
/// non-control bodies so the control roundtrip helper keeps its narrow scope.
///
/// # Errors
///
/// Returns a clear diagnostic for missing, malformed, or non-control request
/// bodies. The caller should surface this as a daemon response instead of
/// dropping the connection.
pub fn supported_control_request_from_prost(
    request: zccache_v1::Request,
) -> Result<super::Request, String> {
    use zccache_v1::request::Body;

    match &request.body {
        Some(
            Body::Ping(_)
            | Body::Status(_)
            | Body::Shutdown(_)
            | Body::Clear(_)
            | Body::ReleaseWorktreeHandles(_),
        ) => request_from_prost(request),
        Some(other) => Err(format!(
            "unsupported v16 prost control request body {other:?}; only Ping, Status, Shutdown, \
             Clear, and ReleaseWorktreeHandles may use the prost control request path"
        )),
        None => Err(
            "unsupported v16 prost request: missing request body; only Ping, Status, Shutdown, \
             Clear, and ReleaseWorktreeHandles may use the prost control request path"
                .to_string(),
        ),
    }
}

/// Convert the narrow daemon-control and maintenance request slice to the v16
/// prost schema.
///
/// # Errors
///
/// Returns a clear diagnostic when a caller tries to route an unsupported
/// request through the prost control path.
pub fn supported_control_request_to_prost(
    request: &super::Request,
) -> Result<zccache_v1::Request, String> {
    match request {
        super::Request::Ping
        | super::Request::Status
        | super::Request::Shutdown
        | super::Request::Clear
        | super::Request::ReleaseWorktreeHandles { .. } => {
            Ok(request_to_prost(request, default_request_id(request)))
        }
        other => Err(format!(
            "unsupported v16 prost control request {other:?}; only Ping, Status, Shutdown, \
             Clear, and ReleaseWorktreeHandles may select {WIRE_FORMAT_ENV} through the prost \
             control request path"
        )),
    }
}

/// Convert v16 prost daemon-control and maintenance responses, rejecting
/// non-control bodies so the control roundtrip helper keeps its narrow scope.
///
/// # Errors
///
/// Returns a clear diagnostic for non-control response bodies or missing
/// nested fields in the supported `Status` response body.
pub fn supported_control_response_from_prost(
    response: zccache_v1::Response,
) -> Result<super::Response, String> {
    use zccache_v1::response::Body;

    match &response.body {
        Some(
            Body::Pong(_)
            | Body::ShuttingDown(_)
            | Body::Status(_)
            | Body::Cleared(_)
            | Body::Error(_)
            | Body::ReleaseWorktreeHandlesResult(_),
        ) => response_from_prost(response),
        Some(other) => Err(format!(
            "unsupported v16 prost control response body {other:?}; only Pong, Status, \
             ShuttingDown, Cleared, Error, and ReleaseWorktreeHandlesResult may use the prost \
             control response path"
        )),
        None => Err(
            "unsupported v16 prost response: missing response body; only Pong, Status, \
             ShuttingDown, Cleared, Error, and ReleaseWorktreeHandlesResult may use the prost \
             control response path"
                .to_string(),
        ),
    }
}

/// Convert the narrow daemon-control and maintenance response slice to the v16
/// prost schema.
///
/// # Errors
///
/// Returns a clear diagnostic when a caller tries to route an unsupported
/// response through the prost control path.
pub fn supported_control_response_to_prost(
    response: &super::Response,
    request_id: &str,
) -> Result<zccache_v1::Response, String> {
    match response {
        super::Response::Pong
        | super::Response::ShuttingDown
        | super::Response::Status(_)
        | super::Response::Cleared { .. }
        | super::Response::Error { .. }
        | super::Response::ReleaseWorktreeHandlesResult { .. } => {
            Ok(response_to_prost(response, request_id))
        }
        other => Err(format!(
            "unsupported v16 prost control response {other:?}; only Pong, Status, \
             ShuttingDown, Cleared, Error, and ReleaseWorktreeHandlesResult may use the prost \
             control response path"
        )),
    }
}

/// Canonical request id used when a request is routed over the v16 prost lane
/// without a caller-supplied id.
#[must_use]
pub const fn default_request_id(request: &super::Request) -> &'static str {
    match request {
        super::Request::Ping => "control-ping",
        super::Request::Status => "control-status",
        super::Request::Shutdown => "control-shutdown",
        super::Request::Clear => "control-clear",
        super::Request::ReleaseWorktreeHandles { .. } => "control-release-worktree-handles",
        super::Request::Lookup { .. } => "lookup",
        super::Request::Store { .. } => "store",
        super::Request::SessionStart { .. } => "session-start",
        super::Request::Compile { .. } => "compile",
        super::Request::SessionEnd { .. } => "session-end",
        super::Request::CompileEphemeral { .. } => "compile-ephemeral",
        super::Request::LinkEphemeral { .. } => "link-ephemeral",
        super::Request::SessionStats { .. } => "session-stats",
        super::Request::FingerprintCheck { .. } => "fingerprint-check",
        super::Request::FingerprintMarkSuccess { .. } => "fingerprint-mark-success",
        super::Request::FingerprintMarkFailure { .. } => "fingerprint-mark-failure",
        super::Request::FingerprintInvalidate { .. } => "fingerprint-invalidate",
        super::Request::ListRustArtifacts => "list-rust-artifacts",
        super::Request::GenericToolExec { .. } => "generic-tool-exec",
    }
}

/// Wire family for full-message-family (non-control) client requests.
///
/// The hot wrapper, session, fingerprint, and exec client paths keep their
/// current v15 bincode default: only an explicit `ZCCACHE_DAEMON_WIRE=prost`
/// opts them into the v16 prost lane. `auto`/unset intentionally stays
/// bincode here (even though the control slice prefers prost under auto) so
/// the staged migration does not change default wire selection. Invalid
/// values also fall back to bincode instead of failing a build.
#[must_use]
pub fn full_family_wire_format_from_env() -> WireFormat {
    match client_wire_selection_from_env() {
        Ok(ClientWireSelection::ProstV16) => WireFormat::ProstV16,
        Ok(ClientWireSelection::Auto | ClientWireSelection::BincodeV15) | Err(_) => {
            WireFormat::BincodeV15
        }
    }
}

/// Convert a dual-wire decoded daemon response into the internal enum.
///
/// v15 bincode responses pass through unchanged; v16 prost responses are
/// converted via [`response_from_prost`].
///
/// # Errors
///
/// Returns a deserialization error when a v16 prost response body is missing
/// or carries malformed required fields.
pub fn response_from_decoded_wire(
    message: super::DecodedWireMessage<super::Response, zccache_v1::Response>,
) -> Result<super::Response, super::ProtocolError> {
    match message {
        super::DecodedWireMessage::BincodeV15(response) => Ok(response),
        super::DecodedWireMessage::ProstV16(response) => {
            response_from_prost(response).map_err(super::ProtocolError::Deserialization)
        }
    }
}

/// Convert any internal daemon request to the v16 prost schema.
#[must_use]
pub fn request_to_prost(request: &super::Request, request_id: &str) -> zccache_v1::Request {
    use zccache_v1::request::Body;

    let body = match request {
        super::Request::Ping => Body::Ping(zccache_v1::Empty {}),
        super::Request::Shutdown => Body::Shutdown(zccache_v1::Empty {}),
        super::Request::Status => Body::Status(zccache_v1::Empty {}),
        super::Request::Clear => Body::Clear(zccache_v1::Empty {}),
        super::Request::Lookup { cache_key } => Body::Lookup(zccache_v1::Lookup {
            cache_key: cache_key.clone(),
        }),
        super::Request::Store {
            cache_key,
            artifact,
        } => Body::Store(zccache_v1::Store {
            cache_key: cache_key.clone(),
            artifact: Some(artifact_data_to_prost(artifact)),
        }),
        super::Request::SessionStart {
            client_pid,
            working_dir,
            log_file,
            track_stats,
            journal_path,
            profile,
            private_daemon,
        } => Body::SessionStart(zccache_v1::SessionStart {
            client_pid: *client_pid,
            working_dir: Some(path_to_prost(working_dir)),
            log_file: log_file.as_ref().map(path_to_prost),
            track_stats: *track_stats,
            journal_path: journal_path.as_ref().map(path_to_prost),
            profile: *profile,
            private_daemon: private_daemon
                .as_ref()
                .map(private_daemon_session_options_to_prost),
        }),
        super::Request::Compile {
            session_id,
            args,
            cwd,
            compiler,
            env,
            stdin,
        } => {
            let (env, env_is_set) = optional_env_to_prost(env.as_deref());
            Body::Compile(zccache_v1::Compile {
                session_id: session_id.clone(),
                args: args.clone(),
                cwd: Some(path_to_prost(cwd)),
                compiler: Some(path_to_prost(compiler)),
                env,
                env_is_set,
                stdin: stdin.clone(),
            })
        }
        super::Request::SessionEnd { session_id } => Body::SessionEnd(zccache_v1::SessionEnd {
            session_id: session_id.clone(),
        }),
        super::Request::CompileEphemeral {
            client_pid,
            working_dir,
            compiler,
            args,
            cwd,
            env,
            stdin,
        } => {
            let (env, env_is_set) = optional_env_to_prost(env.as_deref());
            Body::CompileEphemeral(zccache_v1::CompileEphemeral {
                client_pid: *client_pid,
                working_dir: Some(path_to_prost(working_dir)),
                compiler: Some(path_to_prost(compiler)),
                args: args.clone(),
                cwd: Some(path_to_prost(cwd)),
                env,
                env_is_set,
                stdin: stdin.clone(),
            })
        }
        super::Request::LinkEphemeral {
            client_pid,
            tool,
            args,
            cwd,
            env,
        } => {
            let (env, env_is_set) = optional_env_to_prost(env.as_deref());
            Body::LinkEphemeral(zccache_v1::LinkEphemeral {
                client_pid: *client_pid,
                tool: Some(path_to_prost(tool)),
                args: args.clone(),
                cwd: Some(path_to_prost(cwd)),
                env,
                env_is_set,
            })
        }
        super::Request::SessionStats { session_id } => {
            Body::SessionStats(zccache_v1::SessionStatsRequest {
                session_id: session_id.clone(),
            })
        }
        super::Request::FingerprintCheck {
            cache_file,
            cache_type,
            root,
            extensions,
            include_globs,
            exclude,
        } => Body::FingerprintCheck(zccache_v1::FingerprintCheck {
            cache_file: Some(path_to_prost(cache_file)),
            cache_type: cache_type.clone(),
            root: Some(path_to_prost(root)),
            extensions: extensions.clone(),
            include_globs: include_globs.clone(),
            exclude: exclude.clone(),
        }),
        super::Request::FingerprintMarkSuccess { cache_file } => {
            Body::FingerprintMarkSuccess(zccache_v1::FingerprintMarkSuccess {
                cache_file: Some(path_to_prost(cache_file)),
            })
        }
        super::Request::FingerprintMarkFailure { cache_file } => {
            Body::FingerprintMarkFailure(zccache_v1::FingerprintMarkFailure {
                cache_file: Some(path_to_prost(cache_file)),
            })
        }
        super::Request::FingerprintInvalidate { cache_file } => {
            Body::FingerprintInvalidate(zccache_v1::FingerprintInvalidate {
                cache_file: Some(path_to_prost(cache_file)),
            })
        }
        super::Request::ListRustArtifacts => Body::ListRustArtifacts(zccache_v1::Empty {}),
        super::Request::GenericToolExec {
            tool,
            args,
            cwd,
            env,
            input_files,
            input_extra,
            output_streams,
            output_files,
            tool_hash,
            cache_policy,
            cwd_in_key,
            include_scan_files,
            include_dirs,
            system_include_dirs,
            iquote_dirs,
            depfile,
            non_deterministic,
            key_args_filter,
        } => Body::GenericToolExec(zccache_v1::GenericToolExec {
            tool: Some(path_to_prost(tool)),
            args: args.clone(),
            cwd: Some(path_to_prost(cwd)),
            env: env_pairs_to_prost(env),
            input_files: paths_to_prost(input_files),
            input_extra: input_extra.as_ref().clone(),
            output_streams: Some(exec_output_streams_to_prost(*output_streams)),
            output_files: paths_to_prost(output_files),
            tool_hash: tool_hash.map(|hash| hash.to_vec()),
            cache_policy: exec_cache_policy_to_prost(*cache_policy).into(),
            cwd_in_key: *cwd_in_key,
            include_scan_files: paths_to_prost(include_scan_files),
            include_dirs: paths_to_prost(include_dirs),
            system_include_dirs: paths_to_prost(system_include_dirs),
            iquote_dirs: paths_to_prost(iquote_dirs),
            depfile: depfile.as_ref().map(path_to_prost),
            non_deterministic: *non_deterministic,
            key_args_filter: key_args_filter.clone(),
        }),
        super::Request::ReleaseWorktreeHandles { path } => {
            Body::ReleaseWorktreeHandles(zccache_v1::ReleaseWorktreeHandles {
                path: Some(path_to_prost(path)),
            })
        }
    };

    zccache_v1::Request {
        body: Some(body),
        request_id: request_id.to_string(),
    }
}

/// Convert any v16 prost request to the internal daemon request enum.
///
/// # Errors
///
/// Returns a clear diagnostic for a missing request body, missing required
/// nested fields, or out-of-range enum values. The daemon dispatcher surfaces
/// this as a `Response::Error` instead of dropping the connection.
pub fn request_from_prost(request: zccache_v1::Request) -> Result<super::Request, String> {
    use zccache_v1::request::Body;

    match request.body {
        Some(Body::Ping(_)) => Ok(super::Request::Ping),
        Some(Body::Shutdown(_)) => Ok(super::Request::Shutdown),
        Some(Body::Status(_)) => Ok(super::Request::Status),
        Some(Body::Clear(_)) => Ok(super::Request::Clear),
        Some(Body::Lookup(lookup)) => Ok(super::Request::Lookup {
            cache_key: lookup.cache_key,
        }),
        Some(Body::Store(store)) => Ok(super::Request::Store {
            cache_key: store.cache_key,
            artifact: artifact_data_from_prost(required_prost_field(
                store.artifact,
                "Store.artifact",
            )?)?,
        }),
        Some(Body::SessionStart(start)) => Ok(super::Request::SessionStart {
            client_pid: start.client_pid,
            working_dir: path_from_prost(required_prost_field(
                start.working_dir,
                "SessionStart.working_dir",
            )?),
            log_file: start.log_file.map(path_from_prost),
            track_stats: start.track_stats,
            journal_path: start.journal_path.map(path_from_prost),
            profile: start.profile,
            private_daemon: start
                .private_daemon
                .map(private_daemon_session_options_from_prost),
        }),
        Some(Body::Compile(compile)) => Ok(super::Request::Compile {
            session_id: compile.session_id,
            args: compile.args,
            cwd: path_from_prost(required_prost_field(compile.cwd, "Compile.cwd")?),
            compiler: path_from_prost(required_prost_field(compile.compiler, "Compile.compiler")?),
            env: optional_env_from_prost(compile.env, compile.env_is_set),
            stdin: compile.stdin,
        }),
        Some(Body::SessionEnd(end)) => Ok(super::Request::SessionEnd {
            session_id: end.session_id,
        }),
        Some(Body::CompileEphemeral(compile)) => Ok(super::Request::CompileEphemeral {
            client_pid: compile.client_pid,
            working_dir: path_from_prost(required_prost_field(
                compile.working_dir,
                "CompileEphemeral.working_dir",
            )?),
            compiler: path_from_prost(required_prost_field(
                compile.compiler,
                "CompileEphemeral.compiler",
            )?),
            args: compile.args,
            cwd: path_from_prost(required_prost_field(compile.cwd, "CompileEphemeral.cwd")?),
            env: optional_env_from_prost(compile.env, compile.env_is_set),
            stdin: compile.stdin,
        }),
        Some(Body::LinkEphemeral(link)) => Ok(super::Request::LinkEphemeral {
            client_pid: link.client_pid,
            tool: path_from_prost(required_prost_field(link.tool, "LinkEphemeral.tool")?),
            args: link.args,
            cwd: path_from_prost(required_prost_field(link.cwd, "LinkEphemeral.cwd")?),
            env: optional_env_from_prost(link.env, link.env_is_set),
        }),
        Some(Body::SessionStats(stats)) => Ok(super::Request::SessionStats {
            session_id: stats.session_id,
        }),
        Some(Body::FingerprintCheck(check)) => Ok(super::Request::FingerprintCheck {
            cache_file: path_from_prost(required_prost_field(
                check.cache_file,
                "FingerprintCheck.cache_file",
            )?),
            cache_type: check.cache_type,
            root: path_from_prost(required_prost_field(check.root, "FingerprintCheck.root")?),
            extensions: check.extensions,
            include_globs: check.include_globs,
            exclude: check.exclude,
        }),
        Some(Body::FingerprintMarkSuccess(mark)) => Ok(super::Request::FingerprintMarkSuccess {
            cache_file: path_from_prost(required_prost_field(
                mark.cache_file,
                "FingerprintMarkSuccess.cache_file",
            )?),
        }),
        Some(Body::FingerprintMarkFailure(mark)) => Ok(super::Request::FingerprintMarkFailure {
            cache_file: path_from_prost(required_prost_field(
                mark.cache_file,
                "FingerprintMarkFailure.cache_file",
            )?),
        }),
        Some(Body::FingerprintInvalidate(invalidate)) => {
            Ok(super::Request::FingerprintInvalidate {
                cache_file: path_from_prost(required_prost_field(
                    invalidate.cache_file,
                    "FingerprintInvalidate.cache_file",
                )?),
            })
        }
        Some(Body::ListRustArtifacts(_)) => Ok(super::Request::ListRustArtifacts),
        Some(Body::GenericToolExec(exec)) => Ok(super::Request::GenericToolExec {
            tool: path_from_prost(required_prost_field(exec.tool, "GenericToolExec.tool")?),
            args: exec.args,
            cwd: path_from_prost(required_prost_field(exec.cwd, "GenericToolExec.cwd")?),
            env: env_pairs_from_prost(exec.env),
            input_files: paths_from_prost(exec.input_files),
            input_extra: std::sync::Arc::new(exec.input_extra),
            output_streams: exec_output_streams_from_prost(required_prost_field(
                exec.output_streams,
                "GenericToolExec.output_streams",
            )?),
            output_files: paths_from_prost(exec.output_files),
            tool_hash: tool_hash_from_prost(exec.tool_hash)?,
            cache_policy: exec_cache_policy_from_prost(exec.cache_policy)?,
            cwd_in_key: exec.cwd_in_key,
            include_scan_files: paths_from_prost(exec.include_scan_files),
            include_dirs: paths_from_prost(exec.include_dirs),
            system_include_dirs: paths_from_prost(exec.system_include_dirs),
            iquote_dirs: paths_from_prost(exec.iquote_dirs),
            depfile: exec.depfile.map(path_from_prost),
            non_deterministic: exec.non_deterministic,
            key_args_filter: exec.key_args_filter,
        }),
        Some(Body::ReleaseWorktreeHandles(release)) => Ok(super::Request::ReleaseWorktreeHandles {
            path: path_from_prost(required_prost_field(
                release.path,
                "ReleaseWorktreeHandles.path",
            )?),
        }),
        None => Err("v16 prost request is missing its request body".to_string()),
    }
}

/// Convert any internal daemon response to the v16 prost schema.
#[must_use]
pub fn response_to_prost(response: &super::Response, request_id: &str) -> zccache_v1::Response {
    use zccache_v1::response::Body;

    let body = match response {
        super::Response::Pong => Body::Pong(zccache_v1::Empty {}),
        super::Response::ShuttingDown => Body::ShuttingDown(zccache_v1::Empty {}),
        super::Response::Status(status) => Body::Status(daemon_status_to_prost(status)),
        super::Response::LookupResult(result) => Body::LookupResult(lookup_result_to_prost(result)),
        super::Response::StoreResult(result) => Body::StoreResult(zccache_v1::StoreResult {
            kind: store_result_kind_to_prost(result).into(),
        }),
        super::Response::SessionStarted {
            session_id,
            journal_path,
        } => Body::SessionStarted(zccache_v1::SessionStarted {
            session_id: session_id.clone(),
            journal_path: journal_path.as_ref().map(path_to_prost),
        }),
        super::Response::CompileResult {
            exit_code,
            stdout,
            stderr,
            cached,
        } => Body::CompileResult(zccache_v1::CompileResult {
            exit_code: *exit_code,
            stdout: stdout.as_ref().clone(),
            stderr: stderr.as_ref().clone(),
            cached: *cached,
        }),
        super::Response::SessionEnded { stats } => Body::SessionEnded(zccache_v1::SessionEnded {
            stats: stats.as_ref().map(session_stats_to_prost),
        }),
        super::Response::LinkResult {
            exit_code,
            stdout,
            stderr,
            cached,
            warning,
        } => Body::LinkResult(zccache_v1::LinkResult {
            exit_code: *exit_code,
            stdout: stdout.as_ref().clone(),
            stderr: stderr.as_ref().clone(),
            cached: *cached,
            warning: warning.clone(),
        }),
        super::Response::Error { message } => Body::Error(zccache_v1::Error {
            message: message.clone(),
        }),
        super::Response::Cleared {
            artifacts_removed,
            metadata_cleared,
            dep_graph_contexts_cleared,
            on_disk_bytes_freed,
        } => Body::Cleared(zccache_v1::Cleared {
            artifacts_removed: *artifacts_removed,
            metadata_cleared: *metadata_cleared,
            dep_graph_contexts_cleared: *dep_graph_contexts_cleared,
            on_disk_bytes_freed: *on_disk_bytes_freed,
        }),
        super::Response::SessionStatsResult { stats } => {
            Body::SessionStatsResult(zccache_v1::SessionStatsResult {
                stats: stats.as_ref().map(session_stats_to_prost),
            })
        }
        super::Response::FingerprintCheckResult {
            decision,
            reason,
            changed_files,
        } => Body::FingerprintCheckResult(zccache_v1::FingerprintCheckResult {
            decision: decision.clone(),
            reason: reason.clone(),
            changed_files: changed_files.clone(),
        }),
        super::Response::FingerprintAck => Body::FingerprintAck(zccache_v1::Empty {}),
        super::Response::RustArtifactList { artifacts } => {
            Body::RustArtifactList(zccache_v1::RustArtifactList {
                artifacts: artifacts.iter().map(rust_artifact_info_to_prost).collect(),
            })
        }
        super::Response::GenericToolExecResult {
            exit_code,
            stdout,
            stderr,
            output_files,
            cached,
            cache_key_hex,
        } => Body::GenericToolExecResult(zccache_v1::GenericToolExecResult {
            exit_code: *exit_code,
            stdout: stdout.as_ref().clone(),
            stderr: stderr.as_ref().clone(),
            output_files: output_files.iter().map(artifact_output_to_prost).collect(),
            cached: *cached,
            cache_key_hex: cache_key_hex.clone(),
        }),
        super::Response::Backpressure {
            queue_depth,
            retry_after_ms,
            reason,
        } => Body::Backpressure(zccache_v1::Backpressure {
            queue_depth: *queue_depth,
            retry_after_ms: *retry_after_ms,
            reason: reason.clone(),
        }),
        super::Response::ReleaseWorktreeHandlesResult {
            inspected,
            released,
            sessions_dropped,
            unreleased,
        } => Body::ReleaseWorktreeHandlesResult(zccache_v1::ReleaseWorktreeHandlesResult {
            inspected: *inspected,
            released: *released,
            sessions_dropped: sessions_dropped.clone(),
            unreleased: unreleased.iter().map(path_to_prost).collect(),
        }),
    };

    zccache_v1::Response {
        body: Some(body),
        request_id: request_id.to_string(),
    }
}

/// Convert any v16 prost response to the internal daemon response enum.
///
/// # Errors
///
/// Returns a clear diagnostic for a missing response body, missing required
/// nested fields, or out-of-range enum values.
pub fn response_from_prost(response: zccache_v1::Response) -> Result<super::Response, String> {
    use zccache_v1::response::Body;

    match response.body {
        Some(Body::Pong(_)) => Ok(super::Response::Pong),
        Some(Body::ShuttingDown(_)) => Ok(super::Response::ShuttingDown),
        Some(Body::Status(status)) => daemon_status_from_prost(status).map(super::Response::Status),
        Some(Body::LookupResult(result)) => {
            lookup_result_from_prost(result).map(super::Response::LookupResult)
        }
        Some(Body::StoreResult(result)) => {
            store_result_kind_from_prost(result.kind).map(super::Response::StoreResult)
        }
        Some(Body::SessionStarted(started)) => Ok(super::Response::SessionStarted {
            session_id: started.session_id,
            journal_path: started.journal_path.map(path_from_prost),
        }),
        Some(Body::CompileResult(result)) => Ok(super::Response::CompileResult {
            exit_code: result.exit_code,
            stdout: std::sync::Arc::new(result.stdout),
            stderr: std::sync::Arc::new(result.stderr),
            cached: result.cached,
        }),
        Some(Body::SessionEnded(ended)) => Ok(super::Response::SessionEnded {
            stats: ended.stats.map(session_stats_from_prost),
        }),
        Some(Body::LinkResult(result)) => Ok(super::Response::LinkResult {
            exit_code: result.exit_code,
            stdout: std::sync::Arc::new(result.stdout),
            stderr: std::sync::Arc::new(result.stderr),
            cached: result.cached,
            warning: result.warning,
        }),
        Some(Body::Error(error)) => Ok(super::Response::Error {
            message: error.message,
        }),
        Some(Body::Cleared(cleared)) => Ok(super::Response::Cleared {
            artifacts_removed: cleared.artifacts_removed,
            metadata_cleared: cleared.metadata_cleared,
            dep_graph_contexts_cleared: cleared.dep_graph_contexts_cleared,
            on_disk_bytes_freed: cleared.on_disk_bytes_freed,
        }),
        Some(Body::SessionStatsResult(result)) => Ok(super::Response::SessionStatsResult {
            stats: result.stats.map(session_stats_from_prost),
        }),
        Some(Body::FingerprintCheckResult(result)) => Ok(super::Response::FingerprintCheckResult {
            decision: result.decision,
            reason: result.reason,
            changed_files: result.changed_files,
        }),
        Some(Body::FingerprintAck(_)) => Ok(super::Response::FingerprintAck),
        Some(Body::RustArtifactList(list)) => Ok(super::Response::RustArtifactList {
            artifacts: list
                .artifacts
                .into_iter()
                .map(rust_artifact_info_from_prost)
                .collect::<Result<Vec<_>, _>>()?,
        }),
        Some(Body::GenericToolExecResult(result)) => Ok(super::Response::GenericToolExecResult {
            exit_code: result.exit_code,
            stdout: std::sync::Arc::new(result.stdout),
            stderr: std::sync::Arc::new(result.stderr),
            output_files: result
                .output_files
                .into_iter()
                .map(artifact_output_from_prost)
                .collect::<Result<Vec<_>, _>>()?,
            cached: result.cached,
            cache_key_hex: result.cache_key_hex,
        }),
        Some(Body::Backpressure(backpressure)) => Ok(super::Response::Backpressure {
            queue_depth: backpressure.queue_depth,
            retry_after_ms: backpressure.retry_after_ms,
            reason: backpressure.reason,
        }),
        Some(Body::ReleaseWorktreeHandlesResult(result)) => {
            Ok(super::Response::ReleaseWorktreeHandlesResult {
                inspected: result.inspected,
                released: result.released,
                sessions_dropped: result.sessions_dropped,
                unreleased: result.unreleased.into_iter().map(path_from_prost).collect(),
            })
        }
        None => Err("v16 prost response is missing its response body".to_string()),
    }
}

fn env_pairs_to_prost(env: &[(String, String)]) -> Vec<zccache_v1::EnvVar> {
    env.iter()
        .map(|(name, value)| zccache_v1::EnvVar {
            name: name.clone(),
            value: value.clone(),
        })
        .collect()
}

fn env_pairs_from_prost(env: Vec<zccache_v1::EnvVar>) -> Vec<(String, String)> {
    env.into_iter().map(|var| (var.name, var.value)).collect()
}

fn optional_env_to_prost(env: Option<&[(String, String)]>) -> (Vec<zccache_v1::EnvVar>, bool) {
    match env {
        Some(env) => (env_pairs_to_prost(env), true),
        None => (Vec::new(), false),
    }
}

fn optional_env_from_prost(
    env: Vec<zccache_v1::EnvVar>,
    env_is_set: bool,
) -> Option<Vec<(String, String)>> {
    env_is_set.then(|| env_pairs_from_prost(env))
}

fn paths_to_prost(paths: &[crate::core::NormalizedPath]) -> Vec<zccache_v1::Path> {
    paths.iter().map(path_to_prost).collect()
}

fn paths_from_prost(paths: Vec<zccache_v1::Path>) -> Vec<crate::core::NormalizedPath> {
    paths.into_iter().map(path_from_prost).collect()
}

fn private_daemon_session_options_to_prost(
    options: &super::PrivateDaemonSessionOptions,
) -> zccache_v1::PrivateDaemonSessionOptions {
    zccache_v1::PrivateDaemonSessionOptions {
        daemon_name: options.daemon_name.clone(),
        endpoint: options.endpoint.clone(),
        cache_dir: options.cache_dir.as_ref().map(path_to_prost),
        owner_pids: options.owner_pids.clone(),
        env: env_pairs_to_prost(&options.env),
    }
}

fn private_daemon_session_options_from_prost(
    options: zccache_v1::PrivateDaemonSessionOptions,
) -> super::PrivateDaemonSessionOptions {
    super::PrivateDaemonSessionOptions {
        daemon_name: options.daemon_name,
        endpoint: options.endpoint,
        cache_dir: options.cache_dir.map(path_from_prost),
        owner_pids: options.owner_pids,
        env: env_pairs_from_prost(options.env),
    }
}

fn artifact_data_to_prost(artifact: &super::ArtifactData) -> zccache_v1::ArtifactData {
    zccache_v1::ArtifactData {
        outputs: artifact
            .outputs
            .iter()
            .map(artifact_output_to_prost)
            .collect(),
        stdout: artifact.stdout.as_ref().clone(),
        stderr: artifact.stderr.as_ref().clone(),
        exit_code: artifact.exit_code,
    }
}

fn artifact_data_from_prost(
    artifact: zccache_v1::ArtifactData,
) -> Result<super::ArtifactData, String> {
    Ok(super::ArtifactData {
        outputs: artifact
            .outputs
            .into_iter()
            .map(artifact_output_from_prost)
            .collect::<Result<Vec<_>, _>>()?,
        stdout: std::sync::Arc::new(artifact.stdout),
        stderr: std::sync::Arc::new(artifact.stderr),
        exit_code: artifact.exit_code,
    })
}

fn artifact_output_to_prost(output: &super::ArtifactOutput) -> zccache_v1::ArtifactOutput {
    zccache_v1::ArtifactOutput {
        name: output.name.clone(),
        payload: Some(artifact_payload_to_prost(&output.payload)),
    }
}

fn artifact_output_from_prost(
    output: zccache_v1::ArtifactOutput,
) -> Result<super::ArtifactOutput, String> {
    Ok(super::ArtifactOutput {
        payload: artifact_payload_from_prost(required_prost_field(
            output.payload,
            "ArtifactOutput.payload",
        )?)?,
        name: output.name,
    })
}

fn artifact_payload_to_prost(payload: &super::ArtifactPayload) -> zccache_v1::ArtifactPayload {
    use zccache_v1::artifact_payload::Body;

    zccache_v1::ArtifactPayload {
        body: Some(match payload {
            super::ArtifactPayload::Bytes(bytes) => Body::Bytes(bytes.as_ref().clone()),
            super::ArtifactPayload::Path(path) => Body::Path(path_to_prost(path)),
        }),
    }
}

fn artifact_payload_from_prost(
    payload: zccache_v1::ArtifactPayload,
) -> Result<super::ArtifactPayload, String> {
    use zccache_v1::artifact_payload::Body;

    match payload.body {
        Some(Body::Bytes(bytes)) => Ok(super::ArtifactPayload::Bytes(std::sync::Arc::new(bytes))),
        Some(Body::Path(path)) => Ok(super::ArtifactPayload::Path(path_from_prost(path))),
        None => Err("missing required v16 prost field ArtifactPayload.body".to_string()),
    }
}

fn lookup_result_to_prost(result: &super::LookupResult) -> zccache_v1::LookupResult {
    use zccache_v1::lookup_result::Body;

    zccache_v1::LookupResult {
        body: Some(match result {
            super::LookupResult::Hit { artifact } => Body::Hit(artifact_data_to_prost(artifact)),
            super::LookupResult::Miss => Body::Miss(zccache_v1::Empty {}),
        }),
    }
}

fn lookup_result_from_prost(
    result: zccache_v1::LookupResult,
) -> Result<super::LookupResult, String> {
    use zccache_v1::lookup_result::Body;

    match result.body {
        Some(Body::Hit(artifact)) => Ok(super::LookupResult::Hit {
            artifact: artifact_data_from_prost(artifact)?,
        }),
        Some(Body::Miss(_)) => Ok(super::LookupResult::Miss),
        None => Err("missing required v16 prost field LookupResult.body".to_string()),
    }
}

fn store_result_kind_to_prost(result: &super::StoreResult) -> zccache_v1::StoreResultKind {
    match result {
        super::StoreResult::Stored => zccache_v1::StoreResultKind::Stored,
        super::StoreResult::AlreadyExists => zccache_v1::StoreResultKind::AlreadyExists,
    }
}

fn store_result_kind_from_prost(kind: i32) -> Result<super::StoreResult, String> {
    match zccache_v1::StoreResultKind::try_from(kind) {
        Ok(zccache_v1::StoreResultKind::Stored) => Ok(super::StoreResult::Stored),
        Ok(zccache_v1::StoreResultKind::AlreadyExists) => Ok(super::StoreResult::AlreadyExists),
        Ok(zccache_v1::StoreResultKind::Unspecified) | Err(_) => Err(format!(
            "invalid v16 prost StoreResult.kind value {kind}; expected Stored or AlreadyExists"
        )),
    }
}

fn exec_output_streams_to_prost(
    streams: super::ExecOutputStreams,
) -> zccache_v1::ExecOutputStreams {
    zccache_v1::ExecOutputStreams {
        stdout: streams.stdout,
        stderr: streams.stderr,
    }
}

fn exec_output_streams_from_prost(
    streams: zccache_v1::ExecOutputStreams,
) -> super::ExecOutputStreams {
    super::ExecOutputStreams {
        stdout: streams.stdout,
        stderr: streams.stderr,
    }
}

fn exec_cache_policy_to_prost(policy: super::ExecCachePolicy) -> zccache_v1::ExecCachePolicy {
    match policy {
        super::ExecCachePolicy::Normal => zccache_v1::ExecCachePolicy::Normal,
        super::ExecCachePolicy::Bypass => zccache_v1::ExecCachePolicy::Bypass,
        super::ExecCachePolicy::ReadOnly => zccache_v1::ExecCachePolicy::ReadOnly,
    }
}

fn exec_cache_policy_from_prost(policy: i32) -> Result<super::ExecCachePolicy, String> {
    match zccache_v1::ExecCachePolicy::try_from(policy) {
        Ok(zccache_v1::ExecCachePolicy::Normal) => Ok(super::ExecCachePolicy::Normal),
        Ok(zccache_v1::ExecCachePolicy::Bypass) => Ok(super::ExecCachePolicy::Bypass),
        Ok(zccache_v1::ExecCachePolicy::ReadOnly) => Ok(super::ExecCachePolicy::ReadOnly),
        Ok(zccache_v1::ExecCachePolicy::Unspecified) | Err(_) => Err(format!(
            "invalid v16 prost GenericToolExec.cache_policy value {policy}; \
             expected Normal, Bypass, or ReadOnly"
        )),
    }
}

fn tool_hash_from_prost(hash: Option<Vec<u8>>) -> Result<Option<[u8; 32]>, String> {
    match hash {
        None => Ok(None),
        Some(bytes) => <[u8; 32]>::try_from(bytes.as_slice())
            .map(Some)
            .map_err(|_| {
                format!(
                    "invalid v16 prost GenericToolExec.tool_hash length {}; expected 32 bytes",
                    bytes.len()
                )
            }),
    }
}

fn rust_artifact_info_to_prost(info: &super::RustArtifactInfo) -> zccache_v1::RustArtifactInfo {
    zccache_v1::RustArtifactInfo {
        cache_key: info.cache_key.clone(),
        output_names: info.output_names.clone(),
        payload_count: info.payload_count as u64,
    }
}

fn rust_artifact_info_from_prost(
    info: zccache_v1::RustArtifactInfo,
) -> Result<super::RustArtifactInfo, String> {
    Ok(super::RustArtifactInfo {
        payload_count: usize::try_from(info.payload_count).map_err(|_| {
            format!(
                "invalid v16 prost RustArtifactInfo.payload_count value {}; exceeds usize",
                info.payload_count
            )
        })?,
        cache_key: info.cache_key,
        output_names: info.output_names,
    })
}

fn session_stats_to_prost(stats: &super::SessionStats) -> zccache_v1::SessionStats {
    zccache_v1::SessionStats {
        duration_ms: stats.duration_ms,
        compilations: stats.compilations,
        hits: stats.hits,
        misses: stats.misses,
        non_cacheable: stats.non_cacheable,
        errors: stats.errors,
        errors_cached: stats.errors_cached,
        time_saved_ms: stats.time_saved_ms,
        unique_sources: stats.unique_sources,
        bytes_read: stats.bytes_read,
        bytes_written: stats.bytes_written,
        phase_profile: stats.phase_profile.as_ref().map(phase_profile_to_prost),
    }
}

fn session_stats_from_prost(stats: zccache_v1::SessionStats) -> super::SessionStats {
    super::SessionStats {
        duration_ms: stats.duration_ms,
        compilations: stats.compilations,
        hits: stats.hits,
        misses: stats.misses,
        non_cacheable: stats.non_cacheable,
        errors: stats.errors,
        errors_cached: stats.errors_cached,
        time_saved_ms: stats.time_saved_ms,
        unique_sources: stats.unique_sources,
        bytes_read: stats.bytes_read,
        bytes_written: stats.bytes_written,
        phase_profile: stats.phase_profile.map(phase_profile_from_prost),
    }
}

fn phase_profile_to_prost(profile: &super::PhaseProfileSummary) -> zccache_v1::PhaseProfileSummary {
    zccache_v1::PhaseProfileSummary {
        hit_count: profile.hit_count,
        miss_count: profile.miss_count,
        parse_args_ns: profile.parse_args_ns,
        build_context_ns: profile.build_context_ns,
        hash_source_ns: profile.hash_source_ns,
        hash_headers_ns: profile.hash_headers_ns,
        depgraph_check_ns: profile.depgraph_check_ns,
        request_cache_lookup_ns: profile.request_cache_lookup_ns,
        cross_root_validate_ns: profile.cross_root_validate_ns,
        artifact_lookup_ns: profile.artifact_lookup_ns,
        write_output_ns: profile.write_output_ns,
        bookkeeping_ns: profile.bookkeeping_ns,
        total_hit_ns: profile.total_hit_ns,
        compiler_exec_ns: profile.compiler_exec_ns,
        include_scan_ns: profile.include_scan_ns,
        hash_all_ns: profile.hash_all_ns,
        artifact_store_ns: profile.artifact_store_ns,
        total_miss_ns: profile.total_miss_ns,
    }
}

fn phase_profile_from_prost(
    profile: zccache_v1::PhaseProfileSummary,
) -> super::PhaseProfileSummary {
    super::PhaseProfileSummary {
        hit_count: profile.hit_count,
        miss_count: profile.miss_count,
        parse_args_ns: profile.parse_args_ns,
        build_context_ns: profile.build_context_ns,
        hash_source_ns: profile.hash_source_ns,
        hash_headers_ns: profile.hash_headers_ns,
        depgraph_check_ns: profile.depgraph_check_ns,
        request_cache_lookup_ns: profile.request_cache_lookup_ns,
        cross_root_validate_ns: profile.cross_root_validate_ns,
        artifact_lookup_ns: profile.artifact_lookup_ns,
        write_output_ns: profile.write_output_ns,
        bookkeeping_ns: profile.bookkeeping_ns,
        total_hit_ns: profile.total_hit_ns,
        compiler_exec_ns: profile.compiler_exec_ns,
        include_scan_ns: profile.include_scan_ns,
        hash_all_ns: profile.hash_all_ns,
        artifact_store_ns: profile.artifact_store_ns,
        total_miss_ns: profile.total_miss_ns,
    }
}

fn daemon_status_to_prost(status: &super::DaemonStatus) -> zccache_v1::DaemonStatus {
    zccache_v1::DaemonStatus {
        version: status.version.clone(),
        daemon_namespace: status.daemon_namespace.clone(),
        endpoint: status.endpoint.clone(),
        private_daemon: Some(private_daemon_status_to_prost(&status.private_daemon)),
        artifact_count: status.artifact_count,
        cache_size_bytes: status.cache_size_bytes,
        metadata_entries: status.metadata_entries,
        uptime_secs: status.uptime_secs,
        cache_hits: status.cache_hits,
        cache_misses: status.cache_misses,
        total_compilations: status.total_compilations,
        non_cacheable: status.non_cacheable,
        compile_errors: status.compile_errors,
        compile_errors_cached: status.compile_errors_cached,
        time_saved_ms: status.time_saved_ms,
        total_links: status.total_links,
        link_hits: status.link_hits,
        link_misses: status.link_misses,
        link_non_cacheable: status.link_non_cacheable,
        dep_graph_contexts: status.dep_graph_contexts,
        dep_graph_files: status.dep_graph_files,
        sessions_total: status.sessions_total,
        sessions_active: status.sessions_active,
        cache_dir: Some(path_to_prost(&status.cache_dir)),
        dep_graph_version: status.dep_graph_version,
        dep_graph_disk_size: status.dep_graph_disk_size,
        dep_graph_persisted: status.dep_graph_persisted,
    }
}

fn daemon_status_from_prost(
    status: zccache_v1::DaemonStatus,
) -> Result<super::DaemonStatus, String> {
    Ok(super::DaemonStatus {
        version: status.version,
        daemon_namespace: status.daemon_namespace,
        endpoint: status.endpoint,
        private_daemon: private_daemon_status_from_prost(required_prost_field(
            status.private_daemon,
            "DaemonStatus.private_daemon",
        )?),
        artifact_count: status.artifact_count,
        cache_size_bytes: status.cache_size_bytes,
        metadata_entries: status.metadata_entries,
        uptime_secs: status.uptime_secs,
        cache_hits: status.cache_hits,
        cache_misses: status.cache_misses,
        total_compilations: status.total_compilations,
        non_cacheable: status.non_cacheable,
        compile_errors: status.compile_errors,
        compile_errors_cached: status.compile_errors_cached,
        time_saved_ms: status.time_saved_ms,
        total_links: status.total_links,
        link_hits: status.link_hits,
        link_misses: status.link_misses,
        link_non_cacheable: status.link_non_cacheable,
        dep_graph_contexts: status.dep_graph_contexts,
        dep_graph_files: status.dep_graph_files,
        sessions_total: status.sessions_total,
        sessions_active: status.sessions_active,
        cache_dir: path_from_prost(required_prost_field(
            status.cache_dir,
            "DaemonStatus.cache_dir",
        )?),
        dep_graph_version: status.dep_graph_version,
        dep_graph_disk_size: status.dep_graph_disk_size,
        dep_graph_persisted: status.dep_graph_persisted,
    })
}

fn private_daemon_status_to_prost(
    status: &super::PrivateDaemonStatus,
) -> zccache_v1::PrivateDaemonStatus {
    zccache_v1::PrivateDaemonStatus {
        enabled: status.enabled,
        owners: status
            .owners
            .iter()
            .map(|owner| zccache_v1::PrivateDaemonOwnerStatus {
                pid: owner.pid,
                ref_count: owner.ref_count,
            })
            .collect(),
        private_env_keys: status.private_env_keys.clone(),
    }
}

fn private_daemon_status_from_prost(
    status: zccache_v1::PrivateDaemonStatus,
) -> super::PrivateDaemonStatus {
    super::PrivateDaemonStatus {
        enabled: status.enabled,
        owners: status
            .owners
            .into_iter()
            .map(|owner| super::PrivateDaemonOwnerStatus {
                pid: owner.pid,
                ref_count: owner.ref_count,
            })
            .collect(),
        private_env_keys: status.private_env_keys,
    }
}

fn path_to_prost(path: &crate::core::NormalizedPath) -> zccache_v1::Path {
    zccache_v1::Path {
        value: path.as_path().to_string_lossy().into_owned(),
    }
}

fn path_from_prost(path: zccache_v1::Path) -> crate::core::NormalizedPath {
    crate::core::NormalizedPath::from(path.value)
}

fn required_prost_field<T>(value: Option<T>, field: &str) -> Result<T, String> {
    value.ok_or_else(|| format!("missing required v16 prost field {field}"))
}

/// Serialize a prost message to the planned v16 length-prefixed frame.
///
/// Format: `[4-byte LE length][4-byte LE protocol version][prost payload]`.
/// The length field covers the protocol version plus payload bytes, matching
/// the existing bincode frame envelope.
///
/// # Errors
///
/// Returns an error if prost encoding fails or the payload exceeds the frame
/// size budget.
pub fn encode_prost_message<M: Message>(msg: &M) -> Result<BytesMut, ProtocolError> {
    let mut payload = Vec::with_capacity(msg.encoded_len());
    msg.encode(&mut payload)
        .map_err(|e| ProtocolError::Serialization(e.to_string()))?;

    let frame_len: u32 = (4 + payload.len())
        .try_into()
        .map_err(|_| ProtocolError::MessageTooLarge(payload.len()))?;

    let mut buf = BytesMut::with_capacity(4 + 4 + payload.len());
    buf.put_u32_le(frame_len);
    buf.put_u32_le(PROST_PROTOCOL_VERSION);
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// Try to decode a v16 prost message from a byte buffer.
///
/// Returns `None` when the buffer does not contain a complete frame.
///
/// # Errors
///
/// Returns a version mismatch for non-v16 frames and a deserialization error
/// for malformed prost payloads.
pub fn decode_prost_message<M: Message + Default>(
    buf: &mut BytesMut,
) -> Result<Option<M>, ProtocolError> {
    if buf.len() < 4 {
        return Ok(None);
    }

    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_MESSAGE_SIZE {
        return Err(ProtocolError::MessageTooLarge(len));
    }

    if buf.len() < 4 + len {
        return Ok(None);
    }

    if len < 4 {
        return Err(ProtocolError::Deserialization(
            "frame too small for protocol version".into(),
        ));
    }

    buf.advance(4);
    let frame = buf.split_to(len);

    let remote_ver = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]);
    if remote_ver != PROST_PROTOCOL_VERSION {
        return Err(ProtocolError::VersionMismatch {
            expected: PROST_PROTOCOL_VERSION,
            received: remote_ver,
        });
    }

    M::decode(&frame[4..])
        .map(Some)
        .map_err(|e| ProtocolError::Deserialization(e.to_string()))
}

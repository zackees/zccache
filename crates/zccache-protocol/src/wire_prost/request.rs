//! Full conversion between the internal `Request` enum and the v16 prost
//! `zccache_v1::Request` schema.

use super::convert::{
    artifact_data_from_prost, artifact_data_to_prost, env_pairs_from_prost, env_pairs_to_prost,
    exec_cache_policy_from_prost, exec_cache_policy_to_prost, exec_output_streams_from_prost,
    exec_output_streams_to_prost, optional_env_from_prost, optional_env_to_prost, path_from_prost,
    path_to_prost, paths_from_prost, paths_to_prost, private_daemon_session_options_from_prost,
    private_daemon_session_options_to_prost, required_prost_field, tool_hash_from_prost,
};
use super::zccache_v1;

/// Convert any internal daemon request to the v16 prost schema.
#[must_use]
pub fn request_to_prost(request: &crate::Request, request_id: &str) -> zccache_v1::Request {
    use zccache_v1::request::Body;

    let body = match request {
        crate::Request::Ping => Body::Ping(zccache_v1::Empty {}),
        crate::Request::Shutdown => Body::Shutdown(zccache_v1::Empty {}),
        crate::Request::Status => Body::Status(zccache_v1::Empty {}),
        crate::Request::Clear => Body::Clear(zccache_v1::Empty {}),
        crate::Request::Lookup { cache_key } => Body::Lookup(zccache_v1::Lookup {
            cache_key: cache_key.clone(),
        }),
        crate::Request::Store {
            cache_key,
            artifact,
        } => Body::Store(zccache_v1::Store {
            cache_key: cache_key.clone(),
            artifact: Some(artifact_data_to_prost(artifact)),
        }),
        crate::Request::SessionStart {
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
        crate::Request::Compile {
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
        crate::Request::SessionEnd { session_id } => Body::SessionEnd(zccache_v1::SessionEnd {
            session_id: session_id.clone(),
        }),
        crate::Request::CompileEphemeral {
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
        crate::Request::LinkEphemeral {
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
        crate::Request::SessionStats { session_id } => {
            Body::SessionStats(zccache_v1::SessionStatsRequest {
                session_id: session_id.clone(),
            })
        }
        crate::Request::FingerprintCheck {
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
        crate::Request::FingerprintMarkSuccess { cache_file } => {
            Body::FingerprintMarkSuccess(zccache_v1::FingerprintMarkSuccess {
                cache_file: Some(path_to_prost(cache_file)),
            })
        }
        crate::Request::FingerprintMarkFailure { cache_file } => {
            Body::FingerprintMarkFailure(zccache_v1::FingerprintMarkFailure {
                cache_file: Some(path_to_prost(cache_file)),
            })
        }
        crate::Request::FingerprintInvalidate { cache_file } => {
            Body::FingerprintInvalidate(zccache_v1::FingerprintInvalidate {
                cache_file: Some(path_to_prost(cache_file)),
            })
        }
        crate::Request::ListRustArtifacts => Body::ListRustArtifacts(zccache_v1::Empty {}),
        crate::Request::GenericToolExec {
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
        crate::Request::ReleaseWorktreeHandles { path } => {
            Body::ReleaseWorktreeHandles(zccache_v1::ReleaseWorktreeHandles {
                path: Some(path_to_prost(path)),
            })
        }
        // Issue #838: ExecProbe / ExecStore are bincode-only in slice 1.
        // The prost wire lane will gain proto definitions in a follow-up
        // PR once a wheel consumer needs cross-protocol routing. For now,
        // route a placeholder that the daemon's prost handler rejects;
        // any in-process bincode path is unaffected.
        crate::Request::ExecProbe { .. } | crate::Request::ExecStore { .. } => {
            Body::Ping(zccache_v1::Empty {})
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
pub fn request_from_prost(request: zccache_v1::Request) -> Result<crate::Request, String> {
    use zccache_v1::request::Body;

    match request.body {
        Some(Body::Ping(_)) => Ok(crate::Request::Ping),
        Some(Body::Shutdown(_)) => Ok(crate::Request::Shutdown),
        Some(Body::Status(_)) => Ok(crate::Request::Status),
        Some(Body::Clear(_)) => Ok(crate::Request::Clear),
        Some(Body::Lookup(lookup)) => Ok(crate::Request::Lookup {
            cache_key: lookup.cache_key,
        }),
        Some(Body::Store(store)) => Ok(crate::Request::Store {
            cache_key: store.cache_key,
            artifact: artifact_data_from_prost(required_prost_field(
                store.artifact,
                "Store.artifact",
            )?)?,
        }),
        Some(Body::SessionStart(start)) => Ok(crate::Request::SessionStart {
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
        Some(Body::Compile(compile)) => Ok(crate::Request::Compile {
            session_id: compile.session_id,
            args: compile.args,
            cwd: path_from_prost(required_prost_field(compile.cwd, "Compile.cwd")?),
            compiler: path_from_prost(required_prost_field(compile.compiler, "Compile.compiler")?),
            env: optional_env_from_prost(compile.env, compile.env_is_set),
            stdin: compile.stdin,
        }),
        Some(Body::SessionEnd(end)) => Ok(crate::Request::SessionEnd {
            session_id: end.session_id,
        }),
        Some(Body::CompileEphemeral(compile)) => Ok(crate::Request::CompileEphemeral {
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
        Some(Body::LinkEphemeral(link)) => Ok(crate::Request::LinkEphemeral {
            client_pid: link.client_pid,
            tool: path_from_prost(required_prost_field(link.tool, "LinkEphemeral.tool")?),
            args: link.args,
            cwd: path_from_prost(required_prost_field(link.cwd, "LinkEphemeral.cwd")?),
            env: optional_env_from_prost(link.env, link.env_is_set),
        }),
        Some(Body::SessionStats(stats)) => Ok(crate::Request::SessionStats {
            session_id: stats.session_id,
        }),
        Some(Body::FingerprintCheck(check)) => Ok(crate::Request::FingerprintCheck {
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
        Some(Body::FingerprintMarkSuccess(mark)) => Ok(crate::Request::FingerprintMarkSuccess {
            cache_file: path_from_prost(required_prost_field(
                mark.cache_file,
                "FingerprintMarkSuccess.cache_file",
            )?),
        }),
        Some(Body::FingerprintMarkFailure(mark)) => Ok(crate::Request::FingerprintMarkFailure {
            cache_file: path_from_prost(required_prost_field(
                mark.cache_file,
                "FingerprintMarkFailure.cache_file",
            )?),
        }),
        Some(Body::FingerprintInvalidate(invalidate)) => {
            Ok(crate::Request::FingerprintInvalidate {
                cache_file: path_from_prost(required_prost_field(
                    invalidate.cache_file,
                    "FingerprintInvalidate.cache_file",
                )?),
            })
        }
        Some(Body::ListRustArtifacts(_)) => Ok(crate::Request::ListRustArtifacts),
        Some(Body::GenericToolExec(exec)) => Ok(crate::Request::GenericToolExec {
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
        Some(Body::ReleaseWorktreeHandles(release)) => Ok(crate::Request::ReleaseWorktreeHandles {
            path: path_from_prost(required_prost_field(
                release.path,
                "ReleaseWorktreeHandles.path",
            )?),
        }),
        None => Err("v16 prost request is missing its request body".to_string()),
    }
}

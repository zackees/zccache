//! Canonical child sessions, output draining, and compiler watchdog policy.

use std::io;
use std::process::Output;

use super::{CompilePriority, CompilePriorityDecision};

/// Bound discovery with the same compiler diagnostics and lifecycle policy.
/// Dropping the timed-out session requests native child-tree cleanup.
pub(crate) async fn async_builder_output_with_priority_timeout(
    builder: kernal_api::async_process::AsyncProcessBuilder,
    priority: CompilePriority,
    timeout: std::time::Duration,
    command_description: String,
) -> io::Result<Output> {
    let wait = async_builder_output_with_priority_decision(builder, priority, command_description);
    match kernal_api::async_engine::timeout(timeout, wait).await {
        Ok((result, _decision)) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("child process timed out after {timeout:?}"),
        )),
    }
}

/// Kernel-session process path for linker and tool invocations.
///
/// The session keeps direct-child reaping independent from pipe EOF and gives
/// the orphan-pipe watchdog its existing two-second post-exit grace. Unlike a
/// raw Tokio child, its builder owns containment, stdin closure, and priority
/// at spawn time.
pub(crate) async fn async_builder_output_with_priority(
    builder: kernal_api::async_process::AsyncProcessBuilder,
    priority: CompilePriority,
) -> io::Result<Output> {
    async_builder_output_with_priority_and_post_exit_grace(
        builder,
        priority,
        Some(std::time::Duration::from_secs(2)),
    )
    .await
}

/// Kernel-session process path with an explicit post-exit pipe policy.
/// `None` retains ordinary EOF waiting for known leaf tools; a bounded grace
/// protects compiler/linker-style children from inherited descendant pipes.
pub(crate) async fn async_builder_output_with_priority_and_post_exit_grace(
    builder: kernal_api::async_process::AsyncProcessBuilder,
    priority: CompilePriority,
    post_exit_grace: Option<std::time::Duration>,
) -> io::Result<Output> {
    let (decision, _ticket) = priority.resolve_and_track();
    async_builder_output_with_effective_priority_and_post_exit_grace(
        builder,
        decision.effective,
        post_exit_grace,
    )
    .await
}

/// Compiler-spawn variant which exposes the one admission-time priority
/// decision to the miss profile without sampling a second time.
pub(crate) async fn async_builder_output_with_priority_decision(
    builder: kernal_api::async_process::AsyncProcessBuilder,
    priority: CompilePriority,
    command_description: String,
) -> (io::Result<Output>, CompilePriorityDecision) {
    let (decision, _ticket) = priority.resolve_and_track();
    let result = async_builder_compiler_output_with_effective_priority(
        builder,
        decision.effective,
        None,
        command_description,
        None,
    )
    .await;
    (result, decision)
}

async fn async_builder_output_with_effective_priority_and_post_exit_grace(
    builder: kernal_api::async_process::AsyncProcessBuilder,
    priority: CompilePriority,
    post_exit_grace: Option<std::time::Duration>,
) -> io::Result<Output> {
    use kernal_api::async_process::{
        AsyncProcessSessionEvent, AsyncProcessSessionOptions, StreamKind,
    };

    let priority = kernel_process_priority(priority);
    let options = AsyncProcessSessionOptions {
        max_queued_chunks: 256,
        max_chunk_bytes: 8 * 1024,
        post_exit_grace,
        kill_on_drop: true,
        kill_tree_on_drop: false,
    };
    let mut session = builder
        .stdin(kernal_api::async_process::AsyncStdio::Null)
        .stdout(kernal_api::async_process::AsyncStdio::Piped)
        .stderr(kernal_api::async_process::AsyncStdio::Piped)
        // The native semantic builder installs the equivalent containment on
        // every supported host, including the Windows Job Object path.  This
        // is deliberately not the old Tokio pre-spawn predicate: that
        // predicate only described which hosts needed a post-spawn shim.
        .kill_when_owner_dies(kernel_session_kill_when_owner_dies())
        // Match the former post-spawn helper: privilege denial while raising
        // priority is diagnostic only and never prevents compilation.
        .priority_best_effort(priority)
        // zccache#1562: acquire the shared materialization exclusion on the
        // actor's native spawning thread and retain it through fork/exec.
        // No non-Send guard crosses this caller's await.
        .spawn_admission(kernel_spawn_admission())
        .session(options);
    session.start().await.map_err(async_process_error_to_io)?;
    let (control, mut output) = session.into_parts().map_err(async_process_error_to_io)?;
    let drain = async move {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        while let Some(event) = output.next_output().await {
            match event {
                AsyncProcessSessionEvent::Chunk(chunk) => match chunk.stream {
                    StreamKind::Stdout => stdout.extend_from_slice(&chunk.bytes),
                    StreamKind::Stderr => stderr.extend_from_slice(&chunk.bytes),
                },
                AsyncProcessSessionEvent::StreamAbandoned(stream) => {
                    tracing::warn!(
                        ?stream,
                        "child output pipe remained open after post-exit grace"
                    );
                }
                AsyncProcessSessionEvent::StreamError {
                    kind,
                    message,
                    raw_os_error,
                    ..
                } => {
                    return Err(match raw_os_error {
                        Some(code) => io::Error::from_raw_os_error(code),
                        None => io::Error::new(kind, message),
                    });
                }
                AsyncProcessSessionEvent::StreamEof(_) => {}
            }
        }
        Ok::<_, io::Error>((stdout, stderr))
    };
    let mut drain = std::pin::pin!(drain);
    let wait = control.wait();
    let mut wait = std::pin::pin!(wait);
    match kernal_api::fair_race!((drain.as_mut()), (wait.as_mut())).await {
        kernal_api::async_engine::FairRace2::First(captured) => {
            let (stdout, stderr) = match captured {
                Ok(captured) => captured,
                Err(error) => {
                    // A reader failure can precede direct-child exit. Do not
                    // leave that child behind while `join(wait, drain)` waits
                    // forever for a pipe that will never report completion.
                    cleanup_failed_session(&control, "output read failure").await;
                    return Err(error);
                }
            };
            let status = wait.as_mut().await.map_err(async_process_error_to_io)?;
            Ok(Output {
                status,
                stdout,
                stderr,
            })
        }
        kernal_api::async_engine::FairRace2::Second(status) => {
            let status = status.map_err(async_process_error_to_io)?;
            let (stdout, stderr) = match drain.await {
                Ok(captured) => captured,
                Err(error) => return Err(error),
            };
            Ok(Output {
                status,
                stdout,
                stderr,
            })
        }
    }
}

/// Streaming compiler-session counterpart. Output chunks are forwarded as
/// they are read, while the direct-child lifecycle stays independent from
/// pipe EOF. The result deliberately carries empty byte vectors: the bounded
/// live-output consumer is the sole capture owner, as it was on the former
/// watchdog streaming path.
pub(crate) async fn async_builder_output_streaming_with_priority_decision(
    builder: kernal_api::async_process::AsyncProcessBuilder,
    priority: CompilePriority,
    sender: kernal_api::async_engine::Sender<crate::daemon::compile_output::RawOutputChunk>,
    command_description: String,
) -> (io::Result<Output>, CompilePriorityDecision) {
    let (decision, _ticket) = priority.resolve_and_track();
    let result = async_builder_compiler_output_with_effective_priority(
        builder,
        decision.effective,
        Some(sender),
        command_description,
        None,
    )
    .await;
    (result, decision)
}

/// Feed ephemeral compiler input while independently draining its output.
pub(crate) async fn async_builder_output_with_priority_stdin(
    builder: kernal_api::async_process::AsyncProcessBuilder,
    priority: CompilePriority,
    stdin_bytes: Option<&[u8]>,
    sender: Option<kernal_api::async_engine::Sender<crate::daemon::compile_output::RawOutputChunk>>,
    command_description: String,
) -> io::Result<Output> {
    let (decision, _ticket) = priority.resolve_and_track();
    async_builder_compiler_output_with_effective_priority(
        builder,
        decision.effective,
        sender,
        command_description,
        stdin_bytes,
    )
    .await
}

/// Canonical compiler session loop shared by captured and live-streaming
/// callers.  It retains both watchdog modes and makes exactly one
/// admission-time priority decision in its public wrappers.
async fn async_builder_compiler_output_with_effective_priority(
    builder: kernal_api::async_process::AsyncProcessBuilder,
    priority: CompilePriority,
    sender: Option<kernal_api::async_engine::Sender<crate::daemon::compile_output::RawOutputChunk>>,
    command_description: String,
    stdin_bytes: Option<&[u8]>,
) -> io::Result<Output> {
    use kernal_api::async_process::{AsyncProcessSessionOptions, AsyncStdio};

    let post_exit_grace = compiler_post_exit_grace();
    let options = AsyncProcessSessionOptions {
        max_queued_chunks: 256,
        max_chunk_bytes: 8 * 1024,
        post_exit_grace,
        kill_on_drop: true,
        kill_tree_on_drop: false,
    };
    let input = stdin_bytes.unwrap_or_default();
    let mut session = builder
        .stdin(if input.is_empty() {
            AsyncStdio::Null
        } else {
            AsyncStdio::Piped
        })
        .stdout(AsyncStdio::Piped)
        .stderr(AsyncStdio::Piped)
        .kill_when_owner_dies(kernel_session_kill_when_owner_dies())
        .priority_best_effort(kernel_process_priority(priority))
        .spawn_admission(kernel_spawn_admission())
        .session(options);
    session.start().await.map_err(async_process_error_to_io)?;
    let (control, mut output) = session.into_parts().map_err(async_process_error_to_io)?;
    let write_input = async {
        if !input.is_empty() {
            for chunk in input.chunks(8 * 1024) {
                if control.write_stdin(chunk).await.is_err() {
                    // Preserve the legacy contract: an early stdin close
                    // does not replace the compiler's exit/diagnostics.
                    break;
                }
            }
            let _ = control.close_stdin().await;
        }
    };
    let mut write_input = std::pin::pin!(write_input);
    let mut input_done = false;
    let stall_window = crate::daemon::child_watchdog::stall_window();
    let sample_period = crate::daemon::child_watchdog::stall_tick();
    let mut cpu_samples =
        tokio::time::interval_at(tokio::time::Instant::now() + sample_period, sample_period);
    cpu_samples.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_progress = std::time::Instant::now();
    let mut last_cpu = control.cpu_time().await.ok().flatten();
    let mut stdout_bytes = 0usize;
    let mut stderr_bytes = 0usize;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    let mut stdout_read_error = None;
    let mut stderr_read_error = None;
    let mut orphaned_pipe = false;
    let mut status = None;
    let mut observed_exit_at = None;
    let mut killed_for_stall = false;
    let mut output_closed = false;
    loop {
        if let Some(status) = status {
            while let Some(event) = output.next_output().await {
                match forward_compiler_session_event(
                    event,
                    &sender,
                    &mut stdout_bytes,
                    &mut stderr_bytes,
                    &mut stdout,
                    &mut stderr,
                )
                .await
                {
                    CompilerSessionEvent::PipeReadError { stream, error } => {
                        record_compiler_pipe_error(
                            stream,
                            error,
                            &mut stdout_read_error,
                            &mut stderr_read_error,
                        );
                    }
                    CompilerSessionEvent::StreamAbandoned => orphaned_pipe = true,
                    CompilerSessionEvent::StreamEof(stream) => {
                        mark_stream_eof(stream, &mut stdout_eof, &mut stderr_eof)
                    }
                    CompilerSessionEvent::Progress => {}
                    CompilerSessionEvent::ConsumerDisconnected(error) => return Err(error),
                }
            }
            if killed_for_stall {
                crate::daemon::child_watchdog::deliver_fault_note(
                        &status,
                        stderr_bytes,
                        sender.as_ref(),
                        &mut stderr,
                        &format!(
                            "zccache killed it after {}s with no output and no CPU progress (ZCCACHE_STALL_WINDOW_MS)",
                            stall_window.as_secs(),
                        ),
                    ).await;
            }
            emit_compiler_session_diagnostics(
                &command_description,
                control.pid(),
                status,
                post_exit_grace,
                observed_exit_at.map(|instant: std::time::Instant| instant.elapsed()),
                orphaned_pipe,
                stdout_eof,
                stderr_eof,
                stdout_bytes,
                stderr_bytes,
                stdout_read_error.as_ref(),
                stderr_read_error.as_ref(),
                sender.as_ref(),
                &mut stderr,
            )
            .await;
            return Ok(Output {
                status,
                stdout,
                stderr,
            });
        }

        let wait = control.wait();
        let event = output.next_output();
        let cpu_tick = cpu_samples.tick();
        let mut wait = std::pin::pin!(wait);
        let mut event = std::pin::pin!(event);
        let mut cpu_tick = std::pin::pin!(cpu_tick);
        match kernal_api::fair_race!(
            (write_input.as_mut(); !input_done),
            (wait.as_mut()),
            (event.as_mut(); !output_closed),
            (cpu_tick.as_mut(); !stall_window.is_zero()),
        )
        .await
        {
            kernal_api::async_engine::FairRace4::First(()) => {
                input_done = true;
            }
            kernal_api::async_engine::FairRace4::Second(waited) => {
                status = Some(waited.map_err(async_process_error_to_io)?);
                observed_exit_at = Some(std::time::Instant::now());
            }
            kernal_api::async_engine::FairRace4::Third(event) => match event {
                Some(event) => {
                    match forward_compiler_session_event(
                        event,
                        &sender,
                        &mut stdout_bytes,
                        &mut stderr_bytes,
                        &mut stdout,
                        &mut stderr,
                    )
                    .await
                    {
                        CompilerSessionEvent::Progress => {
                            last_progress = std::time::Instant::now();
                        }
                        CompilerSessionEvent::ConsumerDisconnected(error) => {
                            cleanup_failed_session(&control, "output consumer disconnected").await;
                            return Err(error);
                        }
                        CompilerSessionEvent::StreamEof(stream) => {
                            mark_stream_eof(stream, &mut stdout_eof, &mut stderr_eof)
                        }
                        CompilerSessionEvent::StreamAbandoned => orphaned_pipe = true,
                        CompilerSessionEvent::PipeReadError { stream, error } => {
                            record_compiler_pipe_error(
                                stream,
                                error,
                                &mut stdout_read_error,
                                &mut stderr_read_error,
                            );
                        }
                    }
                }
                None => {
                    output_closed = true;
                }
            },
            kernal_api::async_engine::FairRace4::Fourth(_) => {
                let now_cpu = control.cpu_time().await.ok().flatten();
                let cpu_advanced = session_cpu_time_advanced(last_cpu, now_cpu);
                last_cpu = now_cpu;
                let since_progress = last_progress.elapsed();
                if crate::daemon::child_watchdog::should_kill_stalled(
                    since_progress,
                    stall_window,
                    cpu_advanced,
                ) {
                    crate::daemon::child_watchdog::emit_stall_diagnostics(
                        &command_description,
                        Some(control.pid()),
                        stall_window,
                        since_progress,
                        stdout_bytes,
                        stderr_bytes,
                    );
                    let killed_status = terminate_session(&control).await?;
                    killed_for_stall = true;
                    // Killing the direct child does not consume the
                    // session's queued output. Use the same post-exit
                    // drain as natural exit, retaining trailing bytes,
                    // read errors, and orphan-pipe diagnostics.
                    status = Some(killed_status);
                    observed_exit_at = Some(std::time::Instant::now());
                }
            }
        }
    }
}

pub(super) async fn forward_compiler_session_event(
    event: kernal_api::async_process::AsyncProcessSessionEvent,
    sender: &Option<
        kernal_api::async_engine::Sender<crate::daemon::compile_output::RawOutputChunk>,
    >,
    stdout_bytes: &mut usize,
    stderr_bytes: &mut usize,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
) -> CompilerSessionEvent {
    use kernal_api::async_process::{AsyncProcessSessionEvent, StreamKind};

    match event {
        AsyncProcessSessionEvent::Chunk(chunk) => {
            let output = match chunk.stream {
                StreamKind::Stdout => {
                    *stdout_bytes += chunk.bytes.len();
                    if sender.is_none() {
                        stdout.extend_from_slice(&chunk.bytes);
                    }
                    crate::daemon::compile_output::RawOutputChunk::Stdout(chunk.bytes)
                }
                StreamKind::Stderr => {
                    *stderr_bytes += chunk.bytes.len();
                    if sender.is_none() {
                        stderr.extend_from_slice(&chunk.bytes);
                    }
                    crate::daemon::compile_output::RawOutputChunk::Stderr(chunk.bytes)
                }
            };
            match sender {
                Some(sender) => match sender.send(output).await {
                    Ok(()) => CompilerSessionEvent::Progress,
                    Err(_) => CompilerSessionEvent::ConsumerDisconnected(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "compiler output consumer disconnected",
                    )),
                },
                None => CompilerSessionEvent::Progress,
            }
        }
        AsyncProcessSessionEvent::StreamAbandoned(_) => CompilerSessionEvent::StreamAbandoned,
        AsyncProcessSessionEvent::StreamError {
            stream,
            kind,
            message,
            raw_os_error,
            ..
        } => CompilerSessionEvent::PipeReadError {
            stream,
            error: match raw_os_error {
                Some(code) => io::Error::from_raw_os_error(code),
                None => io::Error::new(kind, message),
            },
        },
        AsyncProcessSessionEvent::StreamEof(stream) => CompilerSessionEvent::StreamEof(stream),
    }
}

pub(super) enum CompilerSessionEvent {
    Progress,
    ConsumerDisconnected(io::Error),
    StreamEof(kernal_api::async_process::StreamKind),
    StreamAbandoned,
    PipeReadError {
        stream: kernal_api::async_process::StreamKind,
        error: io::Error,
    },
}

fn mark_stream_eof(
    stream: kernal_api::async_process::StreamKind,
    stdout_eof: &mut bool,
    stderr_eof: &mut bool,
) {
    match stream {
        kernal_api::async_process::StreamKind::Stdout => *stdout_eof = true,
        kernal_api::async_process::StreamKind::Stderr => *stderr_eof = true,
    }
}

fn record_compiler_pipe_error(
    stream: kernal_api::async_process::StreamKind,
    error: io::Error,
    stdout_error: &mut Option<io::Error>,
    stderr_error: &mut Option<io::Error>,
) {
    let slot = match stream {
        kernal_api::async_process::StreamKind::Stdout => stdout_error,
        kernal_api::async_process::StreamKind::Stderr => stderr_error,
    };
    if slot.is_none() {
        *slot = Some(error);
    }
}

#[allow(clippy::too_many_arguments)]
async fn emit_compiler_session_diagnostics(
    command_description: &str,
    pid: u32,
    status: std::process::ExitStatus,
    post_exit_grace: Option<std::time::Duration>,
    observed_exit_elapsed: Option<std::time::Duration>,
    orphaned_pipe: bool,
    stdout_eof: bool,
    stderr_eof: bool,
    stdout_bytes: usize,
    stderr_bytes: usize,
    stdout_error: Option<&io::Error>,
    stderr_error: Option<&io::Error>,
    sender: Option<
        &kernal_api::async_engine::Sender<crate::daemon::compile_output::RawOutputChunk>,
    >,
    stderr: &mut Vec<u8>,
) {
    if orphaned_pipe {
        if let (Some(grace), Some(elapsed)) = (post_exit_grace, observed_exit_elapsed) {
            crate::daemon::child_watchdog::emit_orphan_pipe_diagnostics(
                command_description,
                Some(pid),
                elapsed,
                grace,
                stdout_bytes,
                stderr_bytes,
                stdout_eof,
                stderr_eof,
            );
        }
    }
    if let Some(error) = stderr_error.or(stdout_error) {
        crate::daemon::child_watchdog::emit_pipe_read_error_diagnostics(
            command_description,
            Some(pid),
            error,
            stdout_error.is_some(),
            stderr_error.is_some(),
            stdout_bytes,
            stderr_bytes,
            &status,
        );
        crate::daemon::child_watchdog::deliver_fault_note(
            &status,
            stderr_bytes,
            sender,
            stderr,
            &format!("reading its output pipe failed ({error})"),
        )
        .await;
    }
}

/// Match the former watchdog's conservative platform policy: unavailable
/// accounting means assumed progress, so an unsupported host never kills a
/// healthy compiler merely because it cannot observe CPU time.
pub(super) fn session_cpu_time_advanced(
    previous: Option<std::time::Duration>,
    current: Option<std::time::Duration>,
) -> bool {
    match (previous, current) {
        (Some(previous), Some(current)) => current > previous,
        _ => true,
    }
}

/// Translate zccache's product policy to canonical host-independent process
/// scheduling intent. The kernel owns the Unix/Windows native mapping.
pub(super) fn kernel_process_priority(
    priority: CompilePriority,
) -> kernal_api::async_process::ProcessPriority {
    use kernal_api::async_process::ProcessPriority;

    match priority {
        CompilePriority::Auto | CompilePriority::Normal => ProcessPriority::Normal,
        CompilePriority::Low => ProcessPriority::Low,
        CompilePriority::Idle => ProcessPriority::Idle,
        CompilePriority::High => ProcessPriority::High,
    }
}

/// Preserve the compiler watchdog's environment contract: zero means disable
/// the post-exit watchdog (ordinary EOF), whereas the session's `Some(ZERO)`
/// intentionally means abandon immediately.
fn compiler_post_exit_grace() -> Option<std::time::Duration> {
    let grace = crate::daemon::child_watchdog::post_exit_grace();
    (!grace.is_zero()).then_some(grace)
}

/// Construct the actor-thread admission hook required to protect staged
/// materialization descriptors during every semantic session fork/exec.
fn kernel_spawn_admission() -> kernal_api::async_process::SpawnAdmission {
    kernal_api::async_process::SpawnAdmission::new(|| {
        Ok::<_, io::Error>(crate::daemon::spawn_exclusion::spawn_shared())
    })
}

/// The semantic session builder, unlike the legacy Tokio bridge, owns the
/// host-specific owner-death implementation at spawn time.
pub(super) fn kernel_session_kill_when_owner_dies() -> bool {
    true
}

/// Keep the error categories and native OS codes that the former Tokio path
/// returned to its callers.  `AsyncProcessError` is a canonical re-export,
/// so matching it here does not couple zccache to a substrate crate.
pub(super) fn async_process_error_to_io(
    error: kernal_api::async_process::AsyncProcessError,
) -> io::Error {
    use kernal_api::async_process::AsyncProcessError;

    match error {
        AsyncProcessError::Spawn(error) | AsyncProcessError::Io(error) => error,
        error @ AsyncProcessError::AlreadyStarted => {
            io::Error::new(io::ErrorKind::AlreadyExists, error)
        }
        error @ (AsyncProcessError::NotRunning | AsyncProcessError::StdinUnavailable) => {
            io::Error::new(io::ErrorKind::BrokenPipe, error)
        }
        error @ AsyncProcessError::RuntimeContext => io::Error::other(error),
        error @ AsyncProcessError::Timeout => io::Error::new(io::ErrorKind::TimedOut, error),
        error @ AsyncProcessError::OutputLimitExceeded { .. } => {
            io::Error::new(io::ErrorKind::FileTooLarge, error)
        }
    }
}

/// Cleanup after an existing operation error; never replace that error or
/// promise reaping when native termination cannot be confirmed.
async fn cleanup_failed_session(
    control: &kernal_api::async_process::AsyncProcessSessionControl,
    context: &'static str,
) {
    if let Err(cleanup_error) = terminate_session(control).await {
        tracing::warn!(%cleanup_error, context, "child cleanup unconfirmed");
    }
}

/// Bound both termination admission and reaping, returning a status only when
/// the native lifecycle confirms it. Dropping a timed-out future does not
/// establish successful cleanup; the caller must retain that uncertainty.
pub(super) async fn terminate_session(
    control: &kernal_api::async_process::AsyncProcessSessionControl,
) -> io::Result<std::process::ExitStatus> {
    let cleanup = async {
        control.kill().await.map_err(async_process_error_to_io)?;
        control.wait().await.map_err(async_process_error_to_io)
    };
    kernal_api::async_engine::timeout(std::time::Duration::from_secs(5), cleanup)
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "child termination deadline elapsed; reaping unconfirmed",
            )
        })?
}

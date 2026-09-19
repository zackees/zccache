//! Canonical child sessions, output draining, and compiler watchdog policy.

use std::io;
use std::process::Output;

use kernal_api::{
    ProcessOutputChunk, ProcessOutputCompletion, ProcessOutputEvent, ProcessPostExitDrain,
    ProcessSessionOptions,
};

use super::{CompilePriority, CompilePriorityDecision};

/// Bound on queued output chunks before the session backpressures the pipes.
const SESSION_MAX_QUEUED_CHUNKS: usize = 256;
/// Bound on one stdin or output chunk.
const SESSION_MAX_CHUNK_BYTES: usize = 8 * 1024;

/// Session bounds shared by every daemon-owned child. `None` keeps strict EOF
/// waiting; `Some(grace)` abandons a descendant-held pipe after `grace`.
fn session_options(post_exit_grace: Option<std::time::Duration>) -> ProcessSessionOptions {
    ProcessSessionOptions {
        max_queued_chunks: SESSION_MAX_QUEUED_CHUNKS,
        max_chunk_bytes: SESSION_MAX_CHUNK_BYTES,
        post_exit_drain: post_exit_grace.map_or(
            ProcessPostExitDrain::WaitForEof,
            ProcessPostExitDrain::AbandonAfter,
        ),
        kill_on_drop: true,
    }
}

/// Bound discovery with the same compiler diagnostics and lifecycle policy.
/// Dropping the timed-out session requests native child-tree cleanup.
pub(crate) async fn async_builder_output_with_priority_timeout(
    builder: kernal_api::SpawnSpec,
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
    builder: kernal_api::SpawnSpec,
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
    builder: kernal_api::SpawnSpec,
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
    builder: kernal_api::SpawnSpec,
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
    builder: kernal_api::SpawnSpec,
    priority: CompilePriority,
    post_exit_grace: Option<std::time::Duration>,
) -> io::Result<Output> {
    use kernal_api::StreamMode;

    let session = builder
        .stdin(StreamMode::Null)
        .stdout(StreamMode::Piped)
        .stderr(StreamMode::Piped)
        // The native semantic builder installs the equivalent containment on
        // every supported host, including the Windows Job Object path.  This
        // is deliberately not the old Tokio pre-spawn predicate: that
        // predicate only described which hosts needed a post-spawn shim.
        .kill_when_owner_dies(kernel_session_kill_when_owner_dies())
        // Match the former post-spawn helper: privilege denial while raising
        // priority is diagnostic only and never prevents compilation.
        .priority_best_effort(kernel_process_priority(priority))
        // zccache#1562: acquire the shared materialization exclusion on the
        // actor's native spawning thread and hold it only across fork/exec.
        // No non-Send guard crosses this caller's await.
        .spawn_admission(kernel_spawn_admission())
        .spawn_session(session_options(post_exit_grace))
        .await?;
    let drain = async {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        while let Some(event) = session.next_output().await {
            match event {
                ProcessOutputEvent::Chunk(ProcessOutputChunk::Stdout(bytes)) => {
                    stdout.extend_from_slice(&bytes);
                }
                ProcessOutputEvent::Chunk(ProcessOutputChunk::Stderr(bytes)) => {
                    stderr.extend_from_slice(&bytes);
                }
                ProcessOutputEvent::Completion(completion) => {
                    match completion_outcome(completion) {
                        SessionCompletion::Eof(_) => {}
                        SessionCompletion::Abandoned(stream) => {
                            tracing::warn!(
                                ?stream,
                                "child output pipe remained open after post-exit grace"
                            );
                        }
                        SessionCompletion::Error { error, .. } => return Err(error),
                    }
                }
            }
        }
        Ok::<_, io::Error>((stdout, stderr))
    };
    let mut drain = std::pin::pin!(drain);
    let wait = session.wait();
    let mut wait = std::pin::pin!(wait);
    match kernal_api::fair_race!((drain.as_mut()), (wait.as_mut())).await {
        kernal_api::async_engine::FairRace2::First(captured) => {
            let (stdout, stderr) = match captured {
                Ok(captured) => captured,
                Err(error) => {
                    // A reader failure can precede direct-child exit. Do not
                    // leave that child behind while `join(wait, drain)` waits
                    // forever for a pipe that will never report completion.
                    cleanup_failed_session(&session, "output read failure").await;
                    return Err(error);
                }
            };
            let status = wait.as_mut().await?.exit_status();
            Ok(Output {
                status,
                stdout,
                stderr,
            })
        }
        kernal_api::async_engine::FairRace2::Second(exit) => {
            let status = exit?.exit_status();
            let (stdout, stderr) = drain.await?;
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
    builder: kernal_api::SpawnSpec,
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
    builder: kernal_api::SpawnSpec,
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
    builder: kernal_api::SpawnSpec,
    priority: CompilePriority,
    sender: Option<kernal_api::async_engine::Sender<crate::daemon::compile_output::RawOutputChunk>>,
    command_description: String,
    stdin_bytes: Option<&[u8]>,
) -> io::Result<Output> {
    use kernal_api::async_engine::{MissedTickBehavior, PeriodicTimer};
    use kernal_api::StreamMode;

    let post_exit_grace = compiler_post_exit_grace();
    let input = stdin_bytes.unwrap_or_default();
    let session = builder
        .stdin(if input.is_empty() {
            StreamMode::Null
        } else {
            StreamMode::Piped
        })
        .stdout(StreamMode::Piped)
        .stderr(StreamMode::Piped)
        .kill_when_owner_dies(kernel_session_kill_when_owner_dies())
        .priority_best_effort(kernel_process_priority(priority))
        .spawn_admission(kernel_spawn_admission())
        .spawn_session(session_options(post_exit_grace))
        .await?;
    let session_pid = session.id();
    let write_input = async {
        if !input.is_empty() {
            for chunk in input.chunks(SESSION_MAX_CHUNK_BYTES) {
                if session.write_stdin(chunk).await.is_err() {
                    // Preserve the legacy contract: an early stdin close
                    // does not replace the compiler's exit/diagnostics.
                    break;
                }
            }
            let _ = session.close_stdin().await;
        }
    };
    let mut write_input = std::pin::pin!(write_input);
    let mut input_done = false;
    let stall_window = crate::daemon::child_watchdog::stall_window();
    let sample_period = crate::daemon::child_watchdog::stall_tick();
    let mut cpu_samples = PeriodicTimer::new_unbounded(sample_period)?;
    cpu_samples.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // The facade timer ticks immediately; the stall monitor's first sample is
    // due one period after spawn, so consume that immediate tick now.
    cpu_samples.tick().await;
    // soldr#3152 / zccache#1586 / #1588: sample the child's memory high-water
    // mark and its live process tree while it runs, publishing to the
    // enclosing compile scope on every return path (the sample publishes from
    // `Drop`). Samples stop once the session has reported the direct child
    // reaped, so a Unix sample never reads a reissued PID; where the host
    // keeps an exited child's peak readable (Windows), it is re-read once.
    let mut memory = crate::daemon::child_watchdog::ChildMemorySample::new();
    let mut memory_samples =
        PeriodicTimer::new_unbounded(crate::daemon::child_watchdog::MEMORY_SAMPLE_TICK)?;
    memory_samples.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_progress = std::time::Instant::now();
    let mut last_cpu = session.cpu_time().await.ok().flatten();
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
            while let Some(event) = session.next_output().await {
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
                session_pid,
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

        let wait = session.wait();
        let event = session.next_output();
        let cpu_tick = cpu_samples.tick();
        let memory_tick = memory_samples.tick();
        let mut wait = std::pin::pin!(wait);
        let mut event = std::pin::pin!(event);
        let mut cpu_tick = std::pin::pin!(cpu_tick);
        let mut memory_tick = std::pin::pin!(memory_tick);
        match kernal_api::fair_race!(
            (write_input.as_mut(); !input_done),
            (wait.as_mut()),
            (event.as_mut(); !output_closed),
            (cpu_tick.as_mut(); !stall_window.is_zero()),
            (memory_tick.as_mut()),
        )
        .await
        {
            kernal_api::async_engine::FairRace5::First(()) => {
                input_done = true;
            }
            kernal_api::async_engine::FairRace5::Second(waited) => {
                status = Some(waited?.exit_status());
                observed_exit_at = Some(std::time::Instant::now());
                if crate::platform::process::inspect::PEAK_RSS_READABLE_AFTER_EXIT {
                    memory.observe(Some(session_pid));
                }
            }
            kernal_api::async_engine::FairRace5::Third(event) => match event {
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
                            cleanup_failed_session(&session, "output consumer disconnected").await;
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
            kernal_api::async_engine::FairRace5::Fourth(()) => {
                let now_cpu = session.cpu_time().await.ok().flatten();
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
                        Some(session_pid),
                        stall_window,
                        since_progress,
                        stdout_bytes,
                        stderr_bytes,
                    );
                    let killed_status = terminate_session(&session).await?;
                    killed_for_stall = true;
                    // Killing the direct child does not consume the
                    // session's queued output. Use the same post-exit
                    // drain as natural exit, retaining trailing bytes,
                    // read errors, and orphan-pipe diagnostics.
                    status = Some(killed_status);
                    observed_exit_at = Some(std::time::Instant::now());
                }
            }
            kernal_api::async_engine::FairRace5::Fifth(()) => memory.observe(Some(session_pid)),
        }
    }
}

pub(super) async fn forward_compiler_session_event(
    event: ProcessOutputEvent,
    sender: &Option<
        kernal_api::async_engine::Sender<crate::daemon::compile_output::RawOutputChunk>,
    >,
    stdout_bytes: &mut usize,
    stderr_bytes: &mut usize,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
) -> CompilerSessionEvent {
    match event {
        ProcessOutputEvent::Chunk(chunk) => {
            let output = match chunk {
                ProcessOutputChunk::Stdout(bytes) => {
                    *stdout_bytes += bytes.len();
                    if sender.is_none() {
                        stdout.extend_from_slice(&bytes);
                    }
                    crate::daemon::compile_output::RawOutputChunk::Stdout(bytes)
                }
                ProcessOutputChunk::Stderr(bytes) => {
                    *stderr_bytes += bytes.len();
                    if sender.is_none() {
                        stderr.extend_from_slice(&bytes);
                    }
                    crate::daemon::compile_output::RawOutputChunk::Stderr(bytes)
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
        ProcessOutputEvent::Completion(completion) => match completion_outcome(completion) {
            SessionCompletion::Eof(stream) => CompilerSessionEvent::StreamEof(stream),
            SessionCompletion::Abandoned(_) => CompilerSessionEvent::StreamAbandoned,
            SessionCompletion::Error { stream, error } => {
                CompilerSessionEvent::PipeReadError { stream, error }
            }
        },
    }
}

pub(super) enum CompilerSessionEvent {
    Progress,
    ConsumerDisconnected(io::Error),
    StreamEof(SessionStream),
    StreamAbandoned,
    PipeReadError {
        stream: SessionStream,
        error: io::Error,
    },
}

/// Which child output pipe a session completion describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SessionStream {
    Stdout,
    Stderr,
}

/// A session stream completion, split into the stream and what happened.
enum SessionCompletion {
    Eof(SessionStream),
    Abandoned(SessionStream),
    Error {
        stream: SessionStream,
        error: io::Error,
    },
}

/// Rebuild a stream fault as the `io::Error` the former Tokio reader
/// returned: the native OS code when the host exposed one, else the portable
/// category and message.
pub(super) fn session_fault_error(
    kind: io::ErrorKind,
    message: &str,
    raw_os_error: Option<i32>,
) -> io::Error {
    match raw_os_error {
        Some(code) => io::Error::from_raw_os_error(code),
        None => io::Error::new(kind, message.to_owned()),
    }
}

fn completion_outcome(completion: ProcessOutputCompletion) -> SessionCompletion {
    let fault_error = |fault: kernal_api::ProcessOutputFault| {
        session_fault_error(fault.kind(), fault.message(), fault.raw_os_error())
    };
    match completion {
        ProcessOutputCompletion::StdoutEof => SessionCompletion::Eof(SessionStream::Stdout),
        ProcessOutputCompletion::StderrEof => SessionCompletion::Eof(SessionStream::Stderr),
        ProcessOutputCompletion::StdoutAbandoned => {
            SessionCompletion::Abandoned(SessionStream::Stdout)
        }
        ProcessOutputCompletion::StderrAbandoned => {
            SessionCompletion::Abandoned(SessionStream::Stderr)
        }
        ProcessOutputCompletion::StdoutError(fault) => SessionCompletion::Error {
            stream: SessionStream::Stdout,
            error: fault_error(fault),
        },
        ProcessOutputCompletion::StderrError(fault) => SessionCompletion::Error {
            stream: SessionStream::Stderr,
            error: fault_error(fault),
        },
    }
}

fn mark_stream_eof(stream: SessionStream, stdout_eof: &mut bool, stderr_eof: &mut bool) {
    match stream {
        SessionStream::Stdout => *stdout_eof = true,
        SessionStream::Stderr => *stderr_eof = true,
    }
}

fn record_compiler_pipe_error(
    stream: SessionStream,
    error: io::Error,
    stdout_error: &mut Option<io::Error>,
    stderr_error: &mut Option<io::Error>,
) {
    let slot = match stream {
        SessionStream::Stdout => stdout_error,
        SessionStream::Stderr => stderr_error,
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
pub(super) fn kernel_process_priority(priority: CompilePriority) -> kernal_api::ProcessPriority {
    use kernal_api::ProcessPriority;

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
fn kernel_spawn_admission() -> kernal_api::SpawnAdmission {
    kernal_api::SpawnAdmission::new(|| {
        Ok::<_, io::Error>(crate::daemon::spawn_exclusion::spawn_shared())
    })
}

/// The semantic session builder, unlike the legacy Tokio bridge, owns the
/// host-specific owner-death implementation at spawn time.
pub(super) fn kernel_session_kill_when_owner_dies() -> bool {
    true
}

/// Cleanup after an existing operation error; never replace that error or
/// promise reaping when native termination cannot be confirmed.
async fn cleanup_failed_session(session: &kernal_api::ProcessSession, context: &'static str) {
    if let Err(cleanup_error) = terminate_session(session).await {
        tracing::warn!(%cleanup_error, context, "child cleanup unconfirmed");
    }
}

/// Bound both termination admission and reaping, returning a status only when
/// the native lifecycle confirms it. Dropping a timed-out future does not
/// establish successful cleanup; the caller must retain that uncertainty.
pub(super) async fn terminate_session(
    session: &kernal_api::ProcessSession,
) -> io::Result<std::process::ExitStatus> {
    let cleanup = async {
        session.kill().await?;
        session
            .wait()
            .await
            .map(kernal_api::ProcessSessionExit::exit_status)
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

//! Bounded live compiler-output plumbing for the embedded API (issue #937).

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;

const DEFAULT_CAPTURE_LIMIT: usize = 1024 * 1024;
const CAPTURE_LIMIT_ENV: &str = "ZCCACHE_STREAM_CAPTURE_LIMIT_BYTES";

#[derive(Debug)]
pub(crate) enum OutputChunk {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

#[derive(Debug)]
pub(crate) enum RawOutputChunk {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

#[derive(Clone)]
pub(crate) struct OutputContext {
    sender: mpsc::Sender<OutputChunk>,
    capture_limit: usize,
    live_compiler: Arc<AtomicBool>,
}

impl OutputContext {
    pub(crate) fn new(sender: mpsc::Sender<OutputChunk>) -> Self {
        Self {
            sender,
            capture_limit: capture_limit(),
            live_compiler: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn mark_live(&self) {
        self.live_compiler.store(true, Ordering::Release);
    }

    pub(crate) fn was_live(&self) -> bool {
        self.live_compiler.load(Ordering::Acquire)
    }
}

tokio::task_local! {
    static OUTPUT_CONTEXT: OutputContext;
}

pub(crate) async fn scope<F: Future>(context: OutputContext, future: F) -> F::Output {
    OUTPUT_CONTEXT.scope(context, future).await
}

pub(crate) fn current() -> Option<OutputContext> {
    OUTPUT_CONTEXT.try_with(Clone::clone).ok()
}

pub(crate) enum StderrFilter<'a> {
    None,
    ShowIncludes { source: &'a Path, cwd: &'a Path },
}

pub(crate) struct CapturedOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) show_includes_scan: Option<crate::depgraph::ScanResult>,
}

pub(crate) async fn consume(
    mut receiver: mpsc::Receiver<RawOutputChunk>,
    context: OutputContext,
    stderr_filter: StderrFilter<'_>,
) -> io::Result<CapturedOutput> {
    context.mark_live();
    let mut stdout = BoundedCapture::new(context.capture_limit, "stdout");
    let mut stderr = BoundedCapture::new(context.capture_limit, "stderr");
    let mut show_includes = match stderr_filter {
        StderrFilter::None => None,
        StderrFilter::ShowIncludes { source, cwd } => Some(
            crate::depgraph::show_includes::ShowIncludesParser::new(source, cwd),
        ),
    };

    while let Some(chunk) = receiver.recv().await {
        match chunk {
            RawOutputChunk::Stdout(bytes) => {
                if let Some(bytes) = stdout.push(&bytes) {
                    send(&context.sender, OutputChunk::Stdout(bytes)).await?;
                }
            }
            RawOutputChunk::Stderr(bytes) => {
                let mut filtered = Vec::new();
                if let Some(parser) = show_includes.as_mut() {
                    parser.push(&bytes, &mut filtered);
                } else {
                    filtered = bytes;
                }
                if let Some(bytes) = stderr.push(&filtered) {
                    send(&context.sender, OutputChunk::Stderr(bytes)).await?;
                }
            }
        }
    }

    let show_includes_scan = if let Some(parser) = show_includes {
        let mut filtered = Vec::new();
        let scan = parser.finish(&mut filtered);
        if let Some(bytes) = stderr.push(&filtered) {
            send(&context.sender, OutputChunk::Stderr(bytes)).await?;
        }
        Some(scan)
    } else {
        None
    };

    if let Some(bytes) = stdout.finish() {
        send(&context.sender, OutputChunk::Stdout(bytes)).await?;
    }
    if let Some(bytes) = stderr.finish() {
        send(&context.sender, OutputChunk::Stderr(bytes)).await?;
    }

    Ok(CapturedOutput {
        stdout: stdout.into_bytes(),
        stderr: stderr.into_bytes(),
        show_includes_scan,
    })
}

async fn send(sender: &mpsc::Sender<OutputChunk>, chunk: OutputChunk) -> io::Result<()> {
    sender.send(chunk).await.map_err(|_| {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            "embedded compile output receiver disconnected",
        )
    })
}

/// Retains a live-emitted head and a ring-buffered tail so truncation keeps
/// both early context and the final diagnostic lines.
struct BoundedCapture {
    bytes: Vec<u8>,
    limit: usize,
    head_limit: usize,
    tail: VecDeque<u8>,
    total: usize,
    stream: &'static str,
}

impl BoundedCapture {
    fn new(limit: usize, stream: &'static str) -> Self {
        let head_limit = limit.div_ceil(2);
        Self {
            bytes: Vec::with_capacity(limit.min(64 * 1024)),
            limit,
            head_limit,
            tail: VecDeque::new(),
            total: 0,
            stream,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Option<Vec<u8>> {
        if bytes.is_empty() {
            return None;
        }
        self.total = self.total.saturating_add(bytes.len());
        let head_remaining = self.head_limit.saturating_sub(self.bytes.len());
        let immediate_len = head_remaining.min(bytes.len());
        let immediate = if immediate_len > 0 {
            self.bytes.extend_from_slice(&bytes[..immediate_len]);
            Some(bytes[..immediate_len].to_vec())
        } else {
            None
        };

        let tail_capacity = self.limit - self.head_limit;
        if tail_capacity > 0 {
            self.tail.extend(&bytes[immediate_len..]);
            let excess = self.tail.len().saturating_sub(tail_capacity);
            self.tail.drain(..excess);
        }
        immediate
    }

    fn finish(&mut self) -> Option<Vec<u8>> {
        if self.tail.is_empty() {
            return None;
        }
        let marker = (self.total > self.limit).then(|| {
            format!(
                "\n[zccache: {} truncated to {} bytes; set {} to change the limit]\n",
                self.stream, self.limit, CAPTURE_LIMIT_ENV
            )
        });
        let mut emitted = Vec::with_capacity(
            self.tail.len() + marker.as_ref().map_or(0, std::string::String::len),
        );
        if let Some(marker) = marker {
            emitted.extend_from_slice(marker.as_bytes());
        }
        emitted.extend(self.tail.drain(..));
        self.bytes.extend_from_slice(&emitted);
        Some(emitted)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

fn capture_limit() -> usize {
    std::env::var(CAPTURE_LIMIT_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(DEFAULT_CAPTURE_LIMIT)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::daemon::process::CompilePriority;

    #[test]
    fn bounded_capture_emits_and_stores_the_same_truncation_marker() {
        let mut capture = BoundedCapture::new(4, "stderr");
        let mut emitted = capture.push(b"abcdef").expect("emitted head");
        emitted.extend(capture.finish().expect("emitted tail"));
        assert_eq!(emitted, capture.into_bytes());
        assert!(String::from_utf8_lossy(&emitted).contains("truncated to 4 bytes"));
        assert!(emitted.ends_with(b"ef"));
    }

    #[test]
    fn capture_below_limit_is_delayed_but_not_truncated() {
        let mut capture = BoundedCapture::new(8, "stdout");
        let mut emitted = capture.push(b"abcdef").expect("emitted head");
        emitted.extend(capture.finish().expect("emitted suffix"));
        assert_eq!(emitted, b"abcdef");
        assert_eq!(emitted, capture.into_bytes());
    }

    #[tokio::test]
    async fn synthetic_large_diagnostic_is_bounded_and_replay_identical() {
        let (sender, mut chunks) = mpsc::channel(8);
        let context = OutputContext {
            sender,
            capture_limit: 1024,
            live_compiler: Arc::new(AtomicBool::new(false)),
        };
        let (raw_sender, raw_receiver) = mpsc::channel(8);
        raw_sender
            .send(RawOutputChunk::Stderr(vec![b'x'; 2 * 1024 * 1024]))
            .await
            .expect("raw output receiver");
        drop(raw_sender);

        let captured = super::consume(raw_receiver, context, StderrFilter::None)
            .await
            .expect("capture");
        let mut emitted = Vec::new();
        while let Ok(chunk) = chunks.try_recv() {
            let OutputChunk::Stderr(bytes) = chunk else {
                panic!("expected stderr")
            };
            emitted.extend(bytes);
        }
        assert_eq!(emitted, captured.stderr);
        assert!(captured.stderr.len() < 2048);
        assert!(String::from_utf8_lossy(&captured.stderr).contains("truncated to 1024 bytes"));
        assert!(captured.stderr.ends_with(&vec![b'x'; 512]));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn first_chunk_arrives_while_child_is_still_running() {
        let (sender, mut chunks) = mpsc::channel(8);
        let context = OutputContext::new(sender);
        let (raw_sender, raw_receiver) = mpsc::channel(8);
        let mut command = tokio::process::Command::new("sh");
        command.args([
            "-c",
            "printf 'first\\n' >&2; sleep 1; printf 'second\\n' >&2",
        ]);

        let process = crate::daemon::process::tokio_command_output_streaming_with_priority_stdin(
            &mut command,
            CompilePriority::Normal,
            None,
            raw_sender,
        );
        let consume = super::consume(raw_receiver, context, StderrFilter::None);
        let operation = async {
            let (process, capture) = tokio::join!(process, consume);
            (
                process.expect("child process"),
                capture.expect("captured output"),
            )
        };
        tokio::pin!(operation);

        let started = std::time::Instant::now();
        let first = tokio::select! {
            chunk = chunks.recv() => chunk.expect("first output chunk"),
            _ = &mut operation => panic!("child exited before first output callback"),
        };
        assert!(started.elapsed() < std::time::Duration::from_millis(800));
        assert!(matches!(first, OutputChunk::Stderr(bytes) if bytes == b"first\n"));

        let (process, capture) = operation.await;
        assert!(process.status.success());
        assert!(started.elapsed() >= std::time::Duration::from_millis(800));
        assert_eq!(capture.stderr, b"first\nsecond\n");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn dropping_streaming_operation_kills_and_reaps_child() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let pid_path = temp.path().join("child.pid");
        let script = format!(
            "echo $$ > '{}'; printf 'started\\n' >&2; exec sleep 30",
            pid_path.display()
        );
        let (sender, mut chunks) = mpsc::channel(8);
        let context = OutputContext::new(sender);
        let operation = async move {
            let (raw_sender, raw_receiver) = mpsc::channel(8);
            let mut command = tokio::process::Command::new("sh");
            command.args(["-c", &script]);
            let process =
                crate::daemon::process::tokio_command_output_streaming_with_priority_stdin(
                    &mut command,
                    CompilePriority::Normal,
                    None,
                    raw_sender,
                );
            let consume = super::consume(raw_receiver, context, StderrFilter::None);
            tokio::join!(process, consume)
        };
        let mut operation = Box::pin(operation);

        let first = tokio::select! {
            chunk = chunks.recv() => chunk.expect("startup output"),
            _ = &mut operation => panic!("child exited before cancellation"),
        };
        assert!(matches!(first, OutputChunk::Stderr(bytes) if bytes == b"started\n"));
        let pid: u32 = std::fs::read_to_string(&pid_path)
            .expect("pid file")
            .trim()
            .parse()
            .expect("pid");

        drop(operation);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while std::path::Path::new(&format!("/proc/{pid}")).exists() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("cancelled child should be reaped");
    }
}

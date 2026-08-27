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

/// Which compiler stream the dependency scanner reads, and whether its lines
/// are ours to remove.
///
/// `clang -H` writes its trace to **stderr**. MSVC / clang-cl `/showIncludes`
/// writes `Note: including file:` to **stdout** (issue #1530 — the daemon used
/// to scan stderr for it, found nothing, and recorded zero dependencies, which
/// turned every subsequent MSVC compile into a stale cache hit after a header
/// edit).
pub(crate) enum StderrFilter<'a> {
    None,
    ShowIncludes {
        source: &'a Path,
        cwd: &'a Path,
        /// True when the daemon added `/showIncludes` itself, so the notes are
        /// its own bookkeeping and must not reach the caller. False when the
        /// caller passed the flag — CMake + Ninja MSVC builds parse those notes
        /// into their own depfiles, so they are scanned but passed through.
        injected: bool,
    },
    HeaderTrace {
        source: &'a Path,
        cwd: &'a Path,
    },
}

impl StderrFilter<'_> {
    /// The stream the parser attaches to. Everything except `/showIncludes`
    /// reads stderr.
    fn reads_stdout(&self) -> bool {
        matches!(self, Self::ShowIncludes { .. })
    }

    /// Whether the scanned lines are removed from the stream the caller sees.
    fn strips_scanned_lines(&self) -> bool {
        match self {
            Self::None => false,
            Self::ShowIncludes { injected, .. } => *injected,
            Self::HeaderTrace { .. } => true,
        }
    }
}

pub(crate) struct CapturedOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) dependency_scan: Option<crate::depgraph::ScanResult>,
}

enum DependencyParser {
    ShowIncludes(crate::depgraph::show_includes::ShowIncludesParser),
    HeaderTrace(crate::depgraph::header_trace::HeaderTraceParser),
}

impl DependencyParser {
    fn push(&mut self, bytes: &[u8], output: &mut Vec<u8>) {
        match self {
            Self::ShowIncludes(parser) => parser.push(bytes, output),
            Self::HeaderTrace(parser) => parser.push(bytes, output),
        }
    }

    fn finish(self, output: &mut Vec<u8>) -> crate::depgraph::ScanResult {
        match self {
            Self::ShowIncludes(parser) => parser.finish(output),
            Self::HeaderTrace(parser) => parser.finish(output),
        }
    }
}

pub(crate) async fn consume(
    mut receiver: mpsc::Receiver<RawOutputChunk>,
    context: OutputContext,
    stderr_filter: StderrFilter<'_>,
) -> io::Result<CapturedOutput> {
    context.mark_live();
    let mut stdout = BoundedCapture::new(context.capture_limit, "stdout");
    let mut stderr = BoundedCapture::new(context.capture_limit, "stderr");
    let parser_reads_stdout = stderr_filter.reads_stdout();
    let strips_scanned_lines = stderr_filter.strips_scanned_lines();
    let mut dependency_parser = match stderr_filter {
        StderrFilter::None => None,
        StderrFilter::ShowIncludes { source, cwd, .. } => Some(DependencyParser::ShowIncludes(
            crate::depgraph::show_includes::ShowIncludesParser::new(source, cwd),
        )),
        StderrFilter::HeaderTrace { source, cwd } => Some(DependencyParser::HeaderTrace(
            crate::depgraph::header_trace::HeaderTraceParser::new(source, cwd),
        )),
    };

    // Run `bytes` through the dependency parser and return what the caller
    // should see: the parser's residue when we own the scanned lines, the
    // original bytes when the caller asked for them itself.
    fn scan_chunk(parser: &mut Option<DependencyParser>, strip: bool, bytes: Vec<u8>) -> Vec<u8> {
        let Some(parser) = parser.as_mut() else {
            return bytes;
        };
        let mut residue = Vec::new();
        parser.push(&bytes, &mut residue);
        if strip {
            residue
        } else {
            bytes
        }
    }

    while let Some(chunk) = receiver.recv().await {
        match chunk {
            RawOutputChunk::Stdout(bytes) => {
                let bytes = if parser_reads_stdout {
                    scan_chunk(&mut dependency_parser, strips_scanned_lines, bytes)
                } else {
                    bytes
                };
                if let Some(bytes) = stdout.push(&bytes) {
                    send(&context.sender, OutputChunk::Stdout(bytes)).await?;
                }
            }
            RawOutputChunk::Stderr(bytes) => {
                let bytes = if parser_reads_stdout {
                    bytes
                } else {
                    scan_chunk(&mut dependency_parser, strips_scanned_lines, bytes)
                };
                if let Some(bytes) = stderr.push(&bytes) {
                    send(&context.sender, OutputChunk::Stderr(bytes)).await?;
                }
            }
        }
    }

    let dependency_scan = if let Some(parser) = dependency_parser {
        // Flush whatever the line splitter is still holding. When we are not
        // stripping, that tail already went through verbatim, so the residue
        // is dropped rather than duplicated.
        let mut residue = Vec::new();
        let scan = parser.finish(&mut residue);
        if strips_scanned_lines {
            let capture = if parser_reads_stdout {
                &mut stdout
            } else {
                &mut stderr
            };
            if let Some(bytes) = capture.push(&residue) {
                let chunk = if parser_reads_stdout {
                    OutputChunk::Stdout(bytes)
                } else {
                    OutputChunk::Stderr(bytes)
                };
                send(&context.sender, chunk).await?;
            }
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
        dependency_scan,
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

    #[tokio::test]
    async fn header_trace_filter_streams_only_real_diagnostics() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let source = temp.path().join("main.c");
        let header = temp.path().join("header with spaces.h");
        std::fs::write(&source, "").expect("source");
        std::fs::write(&header, "").expect("header");
        let trace = format!(". {}\nwarning: retained\n", header.display());
        let (sender, mut chunks) = mpsc::channel(8);
        let context = OutputContext::new(sender);
        let (raw_sender, raw_receiver) = mpsc::channel(128);
        for bytes in trace.as_bytes().chunks(5) {
            raw_sender
                .send(RawOutputChunk::Stderr(bytes.to_vec()))
                .await
                .expect("raw receiver");
        }
        drop(raw_sender);

        let captured = super::consume(
            raw_receiver,
            context,
            StderrFilter::HeaderTrace {
                source: &source,
                cwd: temp.path(),
            },
        )
        .await
        .expect("capture");
        let scan = captured.dependency_scan.expect("header scan");
        let mut emitted = Vec::new();
        while let Ok(chunk) = chunks.try_recv() {
            let OutputChunk::Stderr(bytes) = chunk else {
                panic!("expected stderr")
            };
            emitted.extend(bytes);
        }

        assert_eq!(
            scan.resolved,
            vec![crate::depgraph::depfile::canonicalize_path(
                &header,
                temp.path()
            )]
        );
        assert_eq!(captured.stderr, b"warning: retained\n");
        assert_eq!(emitted, captured.stderr);
    }

    // ── Issue #1530: /showIncludes lives on stdout ──────────────────────

    /// Feed a `/showIncludes` transcript in on one stream and return
    /// `(scan, stdout, stderr)`.
    async fn run_show_includes(
        transcript_on_stdout: bool,
        injected: bool,
        source: &Path,
        cwd: &Path,
        transcript: &str,
    ) -> (Option<crate::depgraph::ScanResult>, Vec<u8>, Vec<u8>) {
        let (sender, _chunks) = mpsc::channel(256);
        let context = OutputContext::new(sender);
        let (raw_sender, raw_receiver) = mpsc::channel(256);
        // Chunk it finely so the line splitter has to reassemble across
        // boundaries, the way a real pipe delivers it.
        for bytes in transcript.as_bytes().chunks(7) {
            let chunk = if transcript_on_stdout {
                RawOutputChunk::Stdout(bytes.to_vec())
            } else {
                RawOutputChunk::Stderr(bytes.to_vec())
            };
            raw_sender.send(chunk).await.expect("raw receiver");
        }
        drop(raw_sender);
        let captured = super::consume(
            raw_receiver,
            context,
            StderrFilter::ShowIncludes {
                source,
                cwd,
                injected,
            },
        )
        .await
        .expect("capture");
        (captured.dependency_scan, captured.stdout, captured.stderr)
    }

    #[tokio::test]
    async fn show_includes_is_scanned_from_stdout_and_stripped_when_injected() {
        // The regression: cl.exe prints these notes on stdout. Scanning
        // stderr recorded zero dependencies, so a header edit did not
        // invalidate the cached object.
        let temp = tempfile::TempDir::new().expect("tempdir");
        let source = temp.path().join("main.c");
        let header = temp.path().join("dep.h");
        std::fs::write(&source, "").expect("source");
        std::fs::write(&header, "").expect("header");
        let transcript = format!("main.c\r\nNote: including file: {}\r\n", header.display());

        let (scan, stdout, stderr) =
            run_show_includes(true, true, &source, temp.path(), &transcript).await;
        let scan = scan.expect("show-includes scan");
        assert_eq!(
            scan.resolved,
            vec![crate::depgraph::depfile::canonicalize_path(
                &header,
                temp.path()
            )]
        );
        assert_eq!(stdout, b"main.c\r\n");
        assert!(stderr.is_empty());
    }

    #[tokio::test]
    async fn show_includes_notes_survive_when_the_caller_asked_for_them() {
        // CMake + Ninja MSVC builds pass /showIncludes themselves and parse
        // the notes into their own depfiles. Stripping them would break the
        // build system's dependency tracking.
        let temp = tempfile::TempDir::new().expect("tempdir");
        let source = temp.path().join("main.c");
        let header = temp.path().join("dep.h");
        std::fs::write(&source, "").expect("source");
        std::fs::write(&header, "").expect("header");
        let transcript = format!("main.c\r\nNote: including file: {}\r\n", header.display());

        let (scan, stdout, _stderr) =
            run_show_includes(true, false, &source, temp.path(), &transcript).await;
        assert_eq!(
            scan.expect("show-includes scan").resolved,
            vec![crate::depgraph::depfile::canonicalize_path(
                &header,
                temp.path()
            )]
        );
        assert_eq!(stdout, transcript.as_bytes());
    }

    #[tokio::test]
    async fn show_includes_leaves_stderr_untouched() {
        // Real diagnostics go to stderr and must pass through whole even
        // though the dependency parser is attached to stdout.
        let temp = tempfile::TempDir::new().expect("tempdir");
        let source = temp.path().join("main.c");
        std::fs::write(&source, "").expect("source");
        let (sender, _chunks) = mpsc::channel(64);
        let context = OutputContext::new(sender);
        let (raw_sender, raw_receiver) = mpsc::channel(64);
        raw_sender
            .send(RawOutputChunk::Stderr(
                b"main.c(1): warning C4101: unreferenced\r\n".to_vec(),
            ))
            .await
            .expect("raw receiver");
        drop(raw_sender);
        let captured = super::consume(
            raw_receiver,
            context,
            StderrFilter::ShowIncludes {
                source: &source,
                cwd: temp.path(),
                injected: true,
            },
        )
        .await
        .expect("capture");
        assert_eq!(
            captured.stderr,
            b"main.c(1): warning C4101: unreferenced\r\n"
        );
    }

    #[tokio::test]
    async fn first_chunk_arrives_while_child_is_still_running() {
        if crate::platform::host::is_windows() {
            return;
        }
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

    #[tokio::test]
    async fn dropping_streaming_operation_kills_and_reaps_child() {
        if !crate::platform::host::is_linux() {
            return;
        }
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

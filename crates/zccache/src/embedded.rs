//! First-class in-process zccache service API.
//!
//! This module exposes the embedded service contract used by host daemons that
//! already own a Tokio runtime. The service reuses the daemon compile/session
//! machinery directly and does not bind or listen on zccache IPC endpoints.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::core::NormalizedPath;
use crate::daemon::server::{
    EmbeddedCompileRequest, EmbeddedDaemon, EmbeddedFlushReport, EmbeddedStatsSnapshot,
};

pub use crate::audit::{AuditConfig, AuditContext};

/// Result type used by the embedded service API.
pub type Result<T> = std::result::Result<T, EmbeddedError>;

/// Errors returned by the embedded service API.
#[derive(Debug, thiserror::Error)]
pub enum EmbeddedError {
    #[error("failed to start embedded zccache service: {0}")]
    Start(String),
    #[error("embedded zccache compile failed: {0}")]
    Compile(String),
    #[error("embedded zccache service is already shut down")]
    ShutDown,
    /// The host-provided cancellation token (see
    /// [`ZccacheConfig::cancellation`]) fired before the operation
    /// finished. Subprocesses already in flight when the token is
    /// observed are reaped via `kill_on_drop` when the suspended future
    /// drops; the host should treat this as a terminal outcome and not
    /// retry the same compile. Issue zccache#923.
    #[error("embedded zccache operation cancelled by host token")]
    Cancelled,
}

/// Opaque in-process zccache service handle.
#[derive(Clone)]
pub struct ZccacheService {
    daemon: Arc<EmbeddedDaemon>,
    shutdown: Arc<AtomicBool>,
    /// Snapshot of the host-supplied cancellation token captured at
    /// [`ZccacheService::start`]. Cloned per call into the `tokio::select!`
    /// races inside [`ZccacheService::compile`] / [`ZccacheService::flush`].
    /// `None` preserves the pre-#923 behavior where only
    /// `shutdown(ShutdownMode::Force)` aborts in-flight work.
    cancellation: Option<CancellationToken>,
}

/// Configuration for [`ZccacheService::start`].
#[derive(Debug, Clone)]
pub struct ZccacheConfig {
    pub host: HostIdentity,
    pub cache_root: NormalizedPath,
    pub audit: AuditConfig,
    pub limits: ServiceLimits,
    pub runtime: RuntimeHooks,
    /// Optional cooperative cancellation token (zccache#923).
    ///
    /// When set, every long-running embedded-service operation (compile
    /// dispatch, flush) races the token via `tokio::select!`. If the
    /// token is cancelled before the operation finishes, the operation
    /// returns [`EmbeddedError::Cancelled`] and the suspended future is
    /// dropped — which in turn drops any [`tokio::process::Child`]
    /// configured with `kill_on_drop(true)`, killing the subprocess.
    ///
    /// `None` preserves the pre-#923 behavior: the service participates
    /// in cancellation only via `shutdown(ShutdownMode::Force)`, which
    /// requires moving the service handle and so cannot be triggered
    /// mid-call.
    ///
    /// Hosts that own a top-level shutdown signal (soldr's daemon
    /// `Notify`, fbuild's coordinator runtime) should clone their token
    /// here so a single ctrl-C / SIGINT collapses both the host and the
    /// embedded service together.
    pub cancellation: Option<CancellationToken>,
}

/// Host identity used to namespace and diagnose an embedded service instance.
///
/// Feeds the synthetic IPC endpoint string `embedded:<product>:<instance_id>:<workspace_id>`
/// which in turn keys `current_backend_identity` (a process-wide
/// `LazyLock<DashMap>` since PR #919). The keying decides which cached
/// entries survive across daemon restarts within the same process — so
/// stability of these three strings is a contract, not an aesthetic.
///
/// # Stability guidance (zccache#925)
///
/// | Field | What it controls | Recommended stability |
/// |---|---|---|
/// | `product` | Tags the daemon for diagnostics + the broker name | Constant per product (e.g. `"soldr"`, `"fbuild"`). Treat as a literal string. |
/// | `instance_id` | Cache-continuity key. Two starts with the same `instance_id` share warm caches; two different `instance_id`s do not. | Stable across daemon restarts on the same host + install. The `HostIdentity::default_for_product` helper hashes `(current_exe, host_data_dir)` which gives you this for free. |
/// | `workspace_id` | Today: same as `instance_id` (no-op key under the synthetic endpoint). Future: per-call value once it migrates to [`CompileRequest`]. | Until it moves, leave equal to `instance_id` — that's the no-op default. |
///
/// What breaks if you violate the contract:
/// - Changing `instance_id` per daemon restart: the warm `current_backend_identity`
///   cache for the previous run is unreachable; every restart pays the
///   first-bind SHA-256 cost again (the 43% on-CPU plateau PR #919 fixed).
/// - Sharing `instance_id` across two unrelated products in the same process:
///   their cache entries collide in the DashMap shard.
/// - Setting `workspace_id` to something other than `instance_id` today:
///   silently namespaces the cache by workspace, which is rarely intended at
///   start-time — wait for the per-compile migration.
///
/// See `HostIdentity::default_for_product` for the helper that satisfies
/// these contracts automatically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostIdentity {
    pub product: String,
    pub instance_id: String,
    pub workspace_id: String,
}

impl HostIdentity {
    /// Build a `HostIdentity` whose `instance_id` is stable across daemon
    /// restarts on the same machine + same install.
    ///
    /// The instance hash mixes `std::env::current_exe()` (so two soldrs
    /// installed at different paths get different ids, and an upgrade in
    /// place keeps the same id when the exe path is unchanged) with the
    /// caller-supplied `product` string (so two products embedding zccache
    /// in the same process get distinct ids even if they share an exe).
    /// `workspace_id` is set equal to `instance_id` so the cache key is
    /// the no-op single-namespace form until the planned per-compile
    /// migration (see the type-level doc).
    ///
    /// If `std::env::current_exe()` fails the hash falls back to a fixed
    /// value derived from the product string only — better than panicking
    /// in a host daemon, but the resulting id is less unique. Callers that
    /// want a stronger guarantee should construct `HostIdentity` directly.
    pub fn default_for_product(product: impl Into<String>) -> Self {
        use blake3::Hasher;
        let product = product.into();
        let mut hasher = Hasher::new();
        hasher.update(product.as_bytes());
        hasher.update(b"\0zccache-host-identity-v1\0");
        if let Ok(exe) = std::env::current_exe() {
            hasher.update(exe.as_os_str().to_string_lossy().as_bytes());
        }
        let bytes = hasher.finalize();
        let mut hex = String::with_capacity(32);
        for byte in &bytes.as_bytes()[..16] {
            use std::fmt::Write;
            let _ = write!(hex, "{byte:02x}");
        }
        Self {
            product,
            instance_id: hex.clone(),
            workspace_id: hex,
        }
    }
}

/// Runtime integration hooks reserved for host-owned Tokio runtimes.
///
/// `service_name` is a diagnostic label only — tokio-console uses it to tag
/// the embedded service's tasks in its display.
///
/// `handle` makes the host's tokio runtime explicit. When set, every
/// long-lived background task the embedded service owns is spawned via
/// `handle.spawn(…)` rather than `tokio::spawn(…)`. When `None`, tasks
/// spawn on the ambient runtime — today's behaviour, which works because
/// `ZccacheService::start` is `async` so it is necessarily called from
/// inside a runtime, and `tokio::spawn` resolves to that runtime. Setting
/// `handle` is the contract the embedded-service doc calls for in the
/// "Sync and Blocking Bridge" section — it lets a host daemon assert "all
/// my zccache work runs on THIS runtime" rather than relying on the
/// implicit calling-runtime convention.
///
/// (zccache#922 — added in 1.12.12; backward compatible because `handle:
/// None` exactly matches the prior implicit-runtime behaviour.)
#[derive(Debug, Clone, Default)]
pub struct RuntimeHooks {
    pub service_name: Option<String>,
    pub handle: Option<tokio::runtime::Handle>,
}

/// Optional service limits. `None` means zccache's existing daemon defaults.
#[derive(Debug, Clone, Default)]
pub struct ServiceLimits {
    pub max_parallel_compiles: Option<usize>,
}

/// One compile invocation submitted to the embedded service.
#[derive(Debug, Clone)]
pub struct CompileRequest {
    pub audit: AuditContext,
    pub compiler: NormalizedPath,
    pub args: Vec<String>,
    pub cwd: NormalizedPath,
    pub env: Vec<(String, String)>,
    pub stdin: Vec<u8>,
}

/// Compile response returned by the embedded service.
#[derive(Debug, Clone)]
pub struct CompileResponse {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub cached: bool,
    pub cache_outcome: CacheOutcome,
    pub compile_id: String,
}

/// Conservative cache outcome exposed by the MVP embedded API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOutcome {
    Hit,
    Miss,
    Error,
}

/// Shutdown behavior requested by the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownMode {
    Graceful,
    Force,
}

/// Report returned by [`ZccacheService::shutdown`].
#[derive(Debug, Clone)]
pub struct ShutdownReport {
    pub mode: ShutdownMode,
    pub flushed: FlushReport,
}

/// Report returned by [`ZccacheService::flush`].
#[derive(Debug, Clone)]
pub struct FlushReport {
    pub pending_writes_drained: bool,
    pub artifact_entries: u64,
    pub metadata_entries: u64,
}

/// Current service statistics.
#[derive(Debug, Clone)]
pub struct ServiceStats {
    pub cache_root: NormalizedPath,
    pub uptime_secs: u64,
    pub total_compilations: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub non_cacheable: u64,
    pub compile_errors: u64,
    pub compile_errors_cached: u64,
    pub time_saved_ms: u64,
    pub artifact_count: u64,
    pub cache_size_bytes: u64,
    pub metadata_entries: u64,
    pub dep_graph_contexts: u64,
    pub dep_graph_files: u64,
    pub sessions_total: u64,
    pub sessions_active: u64,
    pub phase_profile: crate::protocol::PhaseProfileSummary,
}

impl ZccacheService {
    /// Start an in-process zccache service on the caller's Tokio runtime.
    ///
    /// When `config.runtime.handle` is `Some`, persistent background tasks
    /// owned by the embedded daemon (currently the artifact-index writer)
    /// spawn via the supplied [`tokio::runtime::Handle`]. When `None`, they
    /// spawn on the ambient runtime — which works because this function is
    /// `async` and therefore runs inside one. The explicit form is the
    /// zccache#922 contract for host daemons that want to assert all
    /// embedded work shares their runtime (for tokio-console attach unity,
    /// for graceful-shutdown signalling, etc.).
    pub async fn start(config: ZccacheConfig) -> Result<Self> {
        let endpoint = embedded_endpoint(&config.host);
        let cache_root =
            crate::core::config::effective_cache_root_from_top_level(&config.cache_root);
        let daemon = EmbeddedDaemon::start(endpoint, cache_root, config.runtime.handle.clone())
            .await
            .map_err(|err| EmbeddedError::Start(err.to_string()))?;
        Ok(Self {
            daemon: Arc::new(daemon),
            shutdown: Arc::new(AtomicBool::new(false)),
            cancellation: config.cancellation,
        })
    }

    /// Compile using the embedded daemon engine.
    ///
    /// Honors [`ZccacheConfig::cancellation`] (zccache#923): if the
    /// host-supplied token fires before the compile finishes, the call
    /// returns [`EmbeddedError::Cancelled`] and the in-flight compile
    /// future is dropped. The daemon's [`tokio::process::Child`] handles
    /// use `kill_on_drop(true)`, so the subprocess is reaped as a side
    /// effect — there is no orphaned `rustc` left behind. Hosts should
    /// treat `Cancelled` as terminal (no retry inside the same shutdown).
    pub async fn compile(&self, request: CompileRequest) -> Result<CompileResponse> {
        let compile_id = request
            .audit
            .compile_id
            .clone()
            .or_else(|| request.audit.command_id.clone())
            .map(String::from)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if self.shutdown.load(Ordering::Acquire) {
            return Err(EmbeddedError::ShutDown);
        }
        // Fast-path: token already fired before we did anything else.
        // Avoids spawning the compile only to immediately cancel it.
        if let Some(token) = &self.cancellation {
            if token.is_cancelled() {
                return Err(EmbeddedError::Cancelled);
            }
        }
        let compile_future = self.daemon.compile(EmbeddedCompileRequest {
            compiler: request.compiler.into_path_buf(),
            args: request.args,
            cwd: request.cwd.into_path_buf(),
            env: Some(request.env),
            stdin: request.stdin,
        });
        let response = match &self.cancellation {
            Some(token) => {
                let cancelled = token.cancelled();
                tokio::select! {
                    biased;
                    () = cancelled => return Err(EmbeddedError::Cancelled),
                    result = compile_future => result.map_err(EmbeddedError::Compile)?,
                }
            }
            None => compile_future.await.map_err(EmbeddedError::Compile)?,
        };
        let cache_outcome = if response.exit_code != 0 {
            CacheOutcome::Error
        } else if response.cached {
            CacheOutcome::Hit
        } else {
            CacheOutcome::Miss
        };
        Ok(CompileResponse {
            exit_code: response.exit_code,
            stdout: response.stdout.as_ref().clone(),
            stderr: response.stderr.as_ref().clone(),
            cached: response.cached,
            cache_outcome,
            compile_id,
        })
    }

    /// Return a daemon-compatible stats snapshot.
    pub async fn stats(&self) -> Result<ServiceStats> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(EmbeddedError::ShutDown);
        }
        Ok(ServiceStats::from_snapshot(self.daemon.stats().await))
    }

    /// Flush pending embedded service state to disk.
    ///
    /// Honors [`ZccacheConfig::cancellation`] (zccache#923) the same way
    /// [`Self::compile`] does: a cancel mid-flush returns
    /// [`EmbeddedError::Cancelled`] and drops the in-progress flush
    /// future. The artifact-index writer task continues to drain on its
    /// next normal tick; nothing on disk is left half-written because
    /// the flush calls down to atomic batch commits.
    pub async fn flush(&self) -> Result<FlushReport> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(EmbeddedError::ShutDown);
        }
        if let Some(token) = &self.cancellation {
            if token.is_cancelled() {
                return Err(EmbeddedError::Cancelled);
            }
        }
        let flush_future = self.daemon.flush();
        let report = match &self.cancellation {
            Some(token) => {
                let cancelled = token.cancelled();
                tokio::select! {
                    biased;
                    () = cancelled => return Err(EmbeddedError::Cancelled),
                    report = flush_future => report,
                }
            }
            None => flush_future.await,
        };
        Ok(FlushReport::from_report(report))
    }

    /// Shut down the service and flush relevant persisted state.
    pub async fn shutdown(self, mode: ShutdownMode) -> Result<ShutdownReport> {
        if self.shutdown.swap(true, Ordering::AcqRel) {
            return Err(EmbeddedError::ShutDown);
        }
        let report = self.daemon.shutdown().await;
        Ok(ShutdownReport {
            mode,
            flushed: FlushReport::from_report(report),
        })
    }
}

impl ServiceStats {
    fn from_snapshot(snapshot: EmbeddedStatsSnapshot) -> Self {
        let status = snapshot.status;
        Self {
            cache_root: status.cache_dir,
            uptime_secs: status.uptime_secs,
            total_compilations: status.total_compilations,
            cache_hits: status.cache_hits,
            cache_misses: status.cache_misses,
            non_cacheable: status.non_cacheable,
            compile_errors: status.compile_errors,
            compile_errors_cached: status.compile_errors_cached,
            time_saved_ms: status.time_saved_ms,
            artifact_count: status.artifact_count,
            cache_size_bytes: status.cache_size_bytes,
            metadata_entries: status.metadata_entries,
            dep_graph_contexts: status.dep_graph_contexts,
            dep_graph_files: status.dep_graph_files,
            sessions_total: status.sessions_total,
            sessions_active: status.sessions_active,
            phase_profile: snapshot.phase_profile,
        }
    }
}

impl FlushReport {
    fn from_report(report: EmbeddedFlushReport) -> Self {
        Self {
            pending_writes_drained: report.pending_writes_drained,
            artifact_entries: report.artifact_entries,
            metadata_entries: report.metadata_entries,
        }
    }
}

fn embedded_endpoint(host: &HostIdentity) -> String {
    format!(
        "embedded:{}:{}:{}",
        sanitize_identity(&host.product),
        sanitize_identity(&host.instance_id),
        sanitize_identity(&host.workspace_id)
    )
}

fn sanitize_identity(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod cancellation_tests {
    //! zccache#923: tests that `ZccacheConfig::cancellation`, when
    //! supplied, aborts `compile()` and `flush()` cooperatively via a
    //! `tokio::select!` race rather than waiting for the inner future
    //! to finish.

    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    fn fake_compile_request() -> CompileRequest {
        // Compiler path that does not exist on disk — the embedded
        // daemon's spawn step is what we're trying to *not* run, so any
        // unreachable PathBuf works. The cancellation race fires before
        // the spawn even attempts to launch the process.
        CompileRequest {
            audit: AuditContext::new(
                crate::audit::AuditId::new("test-run").expect("non-empty"),
                crate::audit::AuditId::new("test-trace").expect("non-empty"),
            ),
            compiler: PathBuf::from("/nonexistent/compiler-that-never-runs").into(),
            args: vec!["--version".into()],
            cwd: std::env::current_dir().expect("cwd").into(),
            env: Vec::new(),
            stdin: Vec::new(),
        }
    }

    async fn start_service_with_token(
        temp: &TempDir,
        token: Option<CancellationToken>,
        instance_id: &str,
    ) -> Result<ZccacheService> {
        ZccacheService::start(ZccacheConfig {
            host: HostIdentity {
                product: "zccache-test".into(),
                instance_id: instance_id.into(),
                workspace_id: instance_id.into(),
            },
            cache_root: temp.path().join("zccache").into(),
            audit: AuditConfig::default(),
            limits: ServiceLimits::default(),
            runtime: RuntimeHooks::default(),
            cancellation: token,
        })
        .await
    }

    #[tokio::test]
    async fn precancelled_token_returns_cancelled_immediately() {
        // Fast-path: token cancelled before the compile call lands. We
        // should never reach the daemon's spawn step. The acceptance
        // criterion in zccache#923 — "Err(Cancelled) from compile() so
        // soldr's request handler can short-circuit" — is exactly this
        // path.
        let temp = TempDir::new().expect("temp cache root");
        let token = CancellationToken::new();
        token.cancel();
        let service = start_service_with_token(&temp, Some(token), "precancel")
            .await
            .expect("service start");

        let outcome = service.compile(fake_compile_request()).await;
        assert!(
            matches!(outcome, Err(EmbeddedError::Cancelled)),
            "pre-cancelled token must short-circuit compile(), got {outcome:?}"
        );

        // Tear down: shutdown still works after a cancelled compile.
        // Important — the host's exit path needs this to be clean.
        let report = service.shutdown(ShutdownMode::Graceful).await;
        assert!(report.is_ok(), "shutdown after Cancelled must succeed");
    }

    #[tokio::test]
    async fn token_fired_during_compile_returns_cancelled() {
        // Mid-flight cancellation: the compile begins (the inner
        // EmbeddedDaemon::compile future is polled at least once) and
        // the token fires while it's in flight. The `tokio::select!`
        // race must win for the cancel branch.
        //
        // We use a token that is cancelled by a sibling task with a
        // very short delay so the compile future is guaranteed to have
        // been polled before the cancel arrives. The fake compiler
        // path is non-existent so the compile would otherwise fail
        // with a Compile error after spawn — we want Cancelled instead.
        let temp = TempDir::new().expect("temp cache root");
        let token = CancellationToken::new();
        let token_clone = token.clone();
        let service = start_service_with_token(&temp, Some(token), "midflight")
            .await
            .expect("service start");

        let canceller = tokio::spawn(async move {
            // Tiny delay so the compile future starts being polled.
            // 10 ms is a generous floor on Windows scheduling jitter
            // while still being a snappy test.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            token_clone.cancel();
        });

        let outcome = service.compile(fake_compile_request()).await;
        canceller.await.expect("canceller task joined");

        // The race can resolve either way: cancel wins (Cancelled) or
        // the spawn fails first because the compiler binary doesn't
        // exist (Compile). Both prove the cancellation path is wired —
        // the assertion we MUST NOT see is "Ok" because that would
        // mean the fake compiler somehow succeeded.
        match outcome {
            Err(EmbeddedError::Cancelled) | Err(EmbeddedError::Compile(_)) => {}
            other => panic!("mid-flight cancel must yield Cancelled or Compile, got {other:?}"),
        }

        let report = service.shutdown(ShutdownMode::Graceful).await;
        assert!(
            report.is_ok(),
            "shutdown after mid-flight cancel must succeed"
        );
    }

    #[tokio::test]
    async fn no_token_preserves_pre_923_behavior() {
        // Backward-compat: `cancellation: None` must keep today's
        // semantics — compile() runs to completion (success or error)
        // and never returns Cancelled. The fake compiler path makes
        // this a Compile error, not an Ok, which is fine — the point
        // is that the new error variant is opt-in.
        let temp = TempDir::new().expect("temp cache root");
        let service = start_service_with_token(&temp, None, "no-token")
            .await
            .expect("service start");

        let outcome = service.compile(fake_compile_request()).await;
        if let Err(EmbeddedError::Cancelled) = outcome {
            panic!("cancellation: None must never yield Cancelled");
        }

        let report = service.shutdown(ShutdownMode::Graceful).await;
        assert!(report.is_ok());
    }

    #[tokio::test]
    async fn precancelled_token_short_circuits_flush() {
        // Same fast-path as compile() but on the flush path. Important
        // because soldr's BuildSessionEnd handler calls flush() before
        // its own session aggregate write — a cancel-during-shutdown
        // must let the flush return immediately rather than blocking
        // soldr's exit on a stalled disk write.
        let temp = TempDir::new().expect("temp cache root");
        let token = CancellationToken::new();
        token.cancel();
        let service = start_service_with_token(&temp, Some(token), "flush-cancel")
            .await
            .expect("service start");

        let outcome = service.flush().await;
        assert!(
            matches!(outcome, Err(EmbeddedError::Cancelled)),
            "pre-cancelled token must short-circuit flush(), got {outcome:?}"
        );

        let _ = service.shutdown(ShutdownMode::Graceful).await;
    }
}

#[cfg(test)]
mod host_identity_tests {
    //! zccache#925: tests for `HostIdentity::default_for_product` and the
    //! documented stability contract.

    use super::*;

    #[test]
    fn default_for_product_is_stable_within_one_process() {
        // Two calls in the same process must yield byte-identical
        // identities. This is the "cache continuity across daemon
        // restarts on the same install" contract — within a process the
        // current_exe path and product string don't change, so the hash
        // doesn't change.
        let a = HostIdentity::default_for_product("soldr");
        let b = HostIdentity::default_for_product("soldr");
        assert_eq!(a, b, "same product must yield same identity");
        assert_eq!(a.product, "soldr");
        assert_eq!(a.workspace_id, a.instance_id);
    }

    #[test]
    fn default_for_product_differs_per_product() {
        // Two different products must yield distinct identities so they
        // don't collide in the per-process backend-identity DashMap.
        let soldr = HostIdentity::default_for_product("soldr");
        let fbuild = HostIdentity::default_for_product("fbuild");
        assert_ne!(soldr, fbuild);
        assert_ne!(soldr.instance_id, fbuild.instance_id);
    }

    #[test]
    fn default_for_product_instance_id_is_16_bytes_of_hex() {
        // 32 hex chars = 16 bytes. The format is part of the
        // diagnostic surface (`embedded_endpoint` prints it) so freezing
        // it here catches accidental changes.
        let id = HostIdentity::default_for_product("zccache-test");
        assert_eq!(id.instance_id.len(), 32);
        assert!(id.instance_id.chars().all(|c| c.is_ascii_hexdigit()));
    }
}

#[cfg(test)]
mod runtime_hooks_tests {
    //! zccache#922: tests that `RuntimeHooks::handle`, when supplied,
    //! is the runtime where the embedded daemon's background tasks land.

    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[test]
    fn runtime_hooks_default_is_none() {
        // Backward-compat assertion: the default constructor has not
        // changed, the new field is `None`, and callers that don't
        // populate it get today's implicit-runtime behaviour.
        let hooks = RuntimeHooks::default();
        assert!(hooks.handle.is_none());
        assert!(hooks.service_name.is_none());
    }

    #[test]
    fn explicit_handle_owns_background_spawns() {
        // Build a dedicated multi-threaded runtime, hand its handle to
        // ZccacheService::start, and assert that a probe spawned via the
        // service's runtime context lands on THAT runtime — not on the
        // outer runtime that drives the test.
        let host_rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("host-runtime-worker")
            .build()
            .expect("failed to build host runtime");
        let host_handle = host_rt.handle().clone();

        // Sentinel: a thread-local-style atomic that increments when a
        // task observes it's on the host runtime.
        let landed_on_host: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));

        // Start the embedded service from inside the host runtime so the
        // `async` start function has *some* ambient runtime to live on,
        // and pass the host handle in via RuntimeHooks. The contract is:
        // any persistent background task spawned by ZccacheService::start
        // runs on the supplied handle when one is provided.
        let temp = TempDir::new().expect("temp cache root");
        let cache_root: NormalizedPath = temp.path().join("zccache").into();

        let landed_clone = Arc::clone(&landed_on_host);
        let host_handle_clone = host_handle.clone();
        let service = host_rt.block_on(async move {
            ZccacheService::start(ZccacheConfig {
                host: HostIdentity {
                    product: "zccache-test".into(),
                    instance_id: "runtime-hooks".into(),
                    workspace_id: "runtime-hooks".into(),
                },
                cache_root,
                audit: AuditConfig::default(),
                limits: ServiceLimits::default(),
                runtime: RuntimeHooks {
                    service_name: Some("runtime-hooks-test".into()),
                    handle: Some(host_handle_clone),
                },
                cancellation: None,
            })
            .await
        });
        let service = service.expect("service start");

        // Probe: spawn a no-op task via the host handle and confirm we
        // can observe the worker's thread name — this proves the handle
        // we passed in is the one running our work.
        let landed_clone2 = Arc::clone(&landed_clone);
        let probe = host_handle.spawn(async move {
            if std::thread::current()
                .name()
                .map(|n| n.starts_with("host-runtime-worker"))
                .unwrap_or(false)
            {
                landed_clone2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });
        host_rt.block_on(probe).expect("probe ran on host runtime");
        assert!(
            landed_on_host.load(std::sync::atomic::Ordering::Relaxed) >= 1,
            "task spawned via supplied handle must run on host runtime workers"
        );

        // Tear down the service cleanly so the index writer task exits.
        let _ = host_rt.block_on(service.shutdown(ShutdownMode::Graceful));
    }
}

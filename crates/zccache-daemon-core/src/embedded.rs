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
    DiskMaintenanceReport as InternalDiskMaintenanceReport, EmbeddedCompileRequest, EmbeddedDaemon,
    EmbeddedFlushReport, EmbeddedStatsSnapshot, MaintenanceKind as InternalMaintenanceKind,
    MaintenancePolicy, MaintenancePressure as InternalMaintenancePressure,
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
    /// [`ZccacheService::start`]. Compile calls race it via `tokio::select!`;
    /// flush checks only whether it was already latched before persistence
    /// begins, then keeps ownership until the flush completes. `None`
    /// preserves the pre-#923 behavior where only
    /// `shutdown(ShutdownMode::Force)` aborts in-flight work.
    cancellation: Option<CancellationToken>,
    /// RAII handle for the optional host-in-flight counter registration
    /// (zccache#924). Wrapped in `Arc` so the `Clone` impl on
    /// `ZccacheService` does not double-register; the slot is cleared
    /// only when the last clone drops.
    _host_inflight_guard: Option<Arc<crate::daemon::process::HostInFlightGuard>>,
    /// Durable audit JSONL writer (zccache#926). Present when the
    /// `AuditConfig` passed to `start` had `mode > Off`. Held on the
    /// service so its `Drop` keeps the writer task alive for the
    /// service's lifetime; flush + shutdown are forwarded from the
    /// matching `ZccacheService` methods.
    audit_sink: Option<Arc<crate::audit_writer::AuditSink>>,
    /// The host's audit configuration, retained so emitted events can carry
    /// the selected mode and honor the host's redaction policy. `Arc` keeps
    /// `Clone` on the service cheap.
    audit: Arc<AuditConfig>,
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
    /// Compile dispatch races the token via `tokio::select!`. If the token is
    /// cancelled before a compile finishes, the operation returns
    /// [`EmbeddedError::Cancelled`] and the suspended future is dropped —
    /// which in turn drops any [`tokio::process::Child`] configured with
    /// `kill_on_drop(true)`, killing the subprocess. Flush checks for an
    /// already-cancelled token before entering persistence, but an accepted
    /// flush is owned to completion so cancellation cannot strand a partial
    /// checkpoint.
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
        if let Ok(exe) = crate::platform::executable::current_image() {
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
    /// Optional host-supplied in-flight counter (zccache#924).
    ///
    /// When the embedded service runs inside a larger host daemon
    /// (soldr, fbuild) the host typically owns its own spawn machinery
    /// for *its* subprocess children — rustc invocations driven
    /// directly by the host, build tools, etc. zccache's internal
    /// in-flight counter does not see those spawns, so its `Auto`
    /// priority decision underestimates the real subprocess pressure
    /// on the machine: cache-miss compiles get scheduled at `Normal`
    /// even when the host already has dozens of its own rustc children
    /// hammering the CPU.
    ///
    /// Cloning the host's counter here lets `Auto` add
    /// `host_in_flight.load(Acquire)` into its pre-increment count
    /// before deciding `Normal` vs `Low`. The host owns the increment
    /// / decrement protocol on its side; zccache only reads.
    ///
    /// Single-slot contract: only one embedded `ZccacheService` per
    /// process can register a counter at a time. A second registration
    /// overwrites the first and logs a `tracing::warn!` so the
    /// double-register case is debuggable. `None` keeps today's
    /// behavior — `Auto` consults only zccache's internal counter.
    pub host_in_flight: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

/// Optional artifact-store limits for [`ZccacheService::start_with_disk_limits`].
#[derive(Debug, Clone, Default)]
pub struct DiskCacheLimits {
    /// Maximum physical bytes for cached artifacts in this exact service root.
    ///
    /// Mutually exclusive with [`Self::max_cache_percent`]. `None` uses the
    /// dynamic default: 5% of filesystem capacity, clamped to 40-200 GiB and
    /// reduced as needed to preserve the disk-recovery reserve (capped at half
    /// the volume on small filesystems so the cache remains useful). Small
    /// root-local state such as indexes, logs, and daemon metadata is outside
    /// this artifact-store budget.
    pub max_cache_bytes: Option<u64>,
    /// Maximum percentage of filesystem capacity for cached artifacts in this
    /// exact service root.
    ///
    /// Valid values are 1 through 100. Mutually exclusive with
    /// [`Self::max_cache_bytes`].
    pub max_cache_percent: Option<u8>,
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

/// Streaming compile event (issue #937). Yielded by
/// [`ZccacheService::compile_streaming`] as the compiler produces output —
/// `Stdout` and `Stderr` chunks arrive incrementally; the terminal
/// `Done` event carries the exit code and cache outcome.
///
/// Live chunks use a bounded channel and retained output is capped with an
/// explicit truncation marker. Cache hits replay through the same event shape.
#[derive(Debug, Clone)]
pub enum CompileChunk {
    /// A chunk of compiler stdout bytes.
    Stdout(Vec<u8>),
    /// A chunk of compiler stderr bytes.
    Stderr(Vec<u8>),
    /// Terminal event with the compile's outcome metadata.
    Done {
        exit_code: i32,
        cached: bool,
        cache_outcome: CacheOutcome,
        compile_id: String,
    },
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

/// Detailed report returned by [`ZccacheService::flush_detailed`].
///
/// This separate additive type preserves the source-compatible shape of the
/// original public [`FlushReport`] for downstream 1.x users that construct or
/// exhaustively destructure it.
#[derive(Debug, Clone)]
pub struct DetailedFlushReport {
    pub pending_writes_drained: bool,
    pub index_writer_drained: bool,
    pub steps: Vec<FlushStepReport>,
    pub artifact_entries: u64,
    pub metadata_entries: u64,
}

impl DetailedFlushReport {
    /// `true` only when every queued write, index update, and persistence step
    /// completed successfully. Regular flushes bound acquisition of the
    /// publication barrier; once persistence starts its workers are awaited
    /// to completion.
    pub fn is_complete(&self) -> bool {
        self.pending_writes_drained
            && self.index_writer_drained
            && self
                .steps
                .iter()
                .all(|step| matches!(step.outcome, FlushStepOutcome::Completed))
    }
}

/// Detailed report returned by [`ZccacheService::shutdown_detailed`].
#[derive(Debug, Clone)]
pub struct DetailedShutdownReport {
    pub mode: ShutdownMode,
    pub flushed: DetailedFlushReport,
}

/// Outcome of one cache-persistence step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlushStepOutcome {
    Completed,
    Failed(String),
    TimedOut,
}

/// Named result for one cache-persistence step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlushStepReport {
    pub step: String,
    pub outcome: FlushStepOutcome,
}

/// Scope of a host-requested disk-maintenance pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskMaintenanceKind {
    /// Apply disk-pressure thresholds only.
    Pressure,
    /// Also expire entries older than 30 days.
    Full,
}

/// Selects who schedules periodic disk-maintenance passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceOwnership {
    /// The embedded service runs its own pressure and daily full passes.
    Embedded,
    /// The embedding host calls [`ZccacheService::maintain_disk`] from its
    /// existing lifecycle scheduler.
    Host,
}

/// Additive startup options for embedded hosts that need non-default storage
/// or maintenance ownership without expanding [`ZccacheConfig`].
#[derive(Debug, Clone)]
pub struct ZccacheStartOptions {
    pub disk_limits: DiskCacheLimits,
    pub maintenance_ownership: MaintenanceOwnership,
    /// Optional base for private compiler outputs.
    ///
    /// The service creates and cleans only its own `zccache-staging` child
    /// beneath this directory. `None` keeps staging under the cache root.
    pub staging_root: Option<NormalizedPath>,
}

impl Default for ZccacheStartOptions {
    fn default() -> Self {
        Self {
            disk_limits: DiskCacheLimits::default(),
            maintenance_ownership: MaintenanceOwnership::Embedded,
            staging_root: None,
        }
    }
}

/// Pressure tier observed during a disk-maintenance pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskMaintenancePressure {
    None,
    Soft,
    Hard,
}

/// Report returned by [`ZccacheService::maintain_disk`].
#[derive(Debug, Clone)]
pub struct DiskMaintenanceReport {
    pub kind: DiskMaintenanceKind,
    pub pressure: DiskMaintenancePressure,
    /// Resolved artifact-store budget for this pass.
    pub budget_bytes: u64,
    /// Allocated artifact bytes plus in-flight writes before maintenance.
    pub usage_before_bytes: u64,
    /// Allocated artifact bytes plus in-flight writes after maintenance.
    pub usage_after_bytes: u64,
    /// Difference between `usage_before_bytes` and `usage_after_bytes`.
    pub bytes_reclaimed: u64,
    pub artifacts_removed: usize,
    pub expired_artifacts_removed: usize,
    /// In-flight artifact bytes included in both usage measurements.
    pub pending_write_bytes: u64,
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
    /// owned by the embedded daemon (the artifact-index writer plus memory
    /// and disk maintenance loops) spawn via the supplied
    /// [`tokio::runtime::Handle`]. When `None`, they spawn on the ambient
    /// runtime — which works because this function is `async` and therefore
    /// runs inside one. The explicit form is the zccache#922 contract for
    /// host daemons that want to assert all embedded work shares their
    /// runtime (for tokio-console attach unity, graceful-shutdown signalling,
    /// and related diagnostics).
    pub async fn start(config: ZccacheConfig) -> Result<Self> {
        Self::start_with_options(config, ZccacheStartOptions::default()).await
    }

    /// Start an embedded service with an explicit artifact-store budget.
    ///
    /// This separate constructor keeps existing `ServiceLimits` struct
    /// literals source-compatible while allowing hosts to own independent
    /// cache budgets for each exact product root.
    pub async fn start_with_disk_limits(
        config: ZccacheConfig,
        disk_limits: DiskCacheLimits,
    ) -> Result<Self> {
        Self::start_with_options(
            config,
            ZccacheStartOptions {
                disk_limits,
                ..ZccacheStartOptions::default()
            },
        )
        .await
    }

    /// Start an embedded service whose periodic maintenance owner is explicit.
    pub async fn start_with_disk_limits_and_maintenance(
        config: ZccacheConfig,
        disk_limits: DiskCacheLimits,
        maintenance_ownership: MaintenanceOwnership,
    ) -> Result<Self> {
        Self::start_with_options(
            config,
            ZccacheStartOptions {
                disk_limits,
                maintenance_ownership,
                staging_root: None,
            },
        )
        .await
    }

    /// Start an embedded service with additive storage and ownership options.
    ///
    /// Existing constructors preserve their source-compatible defaults;
    /// hosts that need a short private compiler-output root use this method.
    pub async fn start_with_options(
        config: ZccacheConfig,
        options: ZccacheStartOptions,
    ) -> Result<Self> {
        let endpoint = embedded_endpoint(&config.host);
        let cache_root =
            crate::core::config::effective_cache_root_from_top_level(&config.cache_root);
        let maintenance_policy = MaintenancePolicy::from_limits(
            options.disk_limits.max_cache_bytes,
            options.disk_limits.max_cache_percent,
        )
        .map_err(EmbeddedError::Start)?;
        let daemon = EmbeddedDaemon::start_with_maintenance(
            endpoint,
            cache_root,
            options.staging_root.as_ref(),
            config.runtime.handle.clone(),
            maintenance_policy,
            options.maintenance_ownership == MaintenanceOwnership::Embedded,
        )
        .await
        .map_err(|err| EmbeddedError::Start(err.to_string()))?;
        // zccache#924: register the optional host-in-flight counter so
        // CompilePriority::Auto sees host-side subprocess pressure when
        // deciding Normal vs Low. The RAII guard is held on the service
        // until the last clone drops, then the slot is cleared.
        let host_inflight_guard = config
            .limits
            .host_in_flight
            .map(crate::daemon::process::register_host_in_flight_counter)
            .map(Arc::new);
        // zccache#926: spawn the durable audit JSONL writer when the
        // host configured a mode that requires emission. The writer
        // task runs on the host's tokio runtime via the same
        // `runtime.handle` plumbing as the rest of the embedded
        // service so tokio-console attach unity holds.
        let audit_sink =
            crate::audit_writer::AuditSink::start(&config.audit, config.runtime.handle.clone())
                .map_err(|err| EmbeddedError::Start(err.to_string()))?
                .map(Arc::new);
        Ok(Self {
            daemon: Arc::new(daemon),
            shutdown: Arc::new(AtomicBool::new(false)),
            cancellation: config.cancellation,
            _host_inflight_guard: host_inflight_guard,
            audit_sink,
            audit: Arc::new(config.audit),
        })
    }

    /// Emit one durable audit event, if the host enabled audit.
    ///
    /// #905: the sink was started, flushed and shut down but nothing ever
    /// called `emit`, so a host that configured `audit.jsonl` got a file that
    /// was created, rotated and always empty. This is the seam that feeds it.
    ///
    /// Costs one `Option` check when audit is off (`AuditMode::Off` makes
    /// `AuditSink::start` return `None`), so the default path is unaffected.
    ///
    /// A failed emit is dropped, never propagated: losing an audit record must
    /// not fail a compile that otherwise succeeded. `AuditSink` already owns
    /// the backpressure policy and its own lost-event counter, so a caller
    /// here has nothing useful to add.
    fn emit_audit(
        &self,
        context: &AuditContext,
        category: &'static str,
        event: &'static str,
        level: crate::audit::AuditLevel,
        duration_ns: Option<u64>,
        fields: &[(&'static str, serde_json::Value)],
    ) {
        let Some(sink) = &self.audit_sink else {
            return;
        };
        let (Ok(event_id), Ok(span_id), Ok(category), Ok(event)) = (
            crate::audit::AuditId::new(uuid::Uuid::new_v4().to_string()),
            crate::audit::AuditId::new(uuid::Uuid::new_v4().to_string()),
            crate::audit::AuditCategory::new(category),
            crate::audit::AuditEventName::new(event),
        ) else {
            // Every argument is a compile-time constant or a fresh UUID, so
            // this is unreachable in practice; returning beats unwrapping in
            // a path that must never take down a compile.
            return;
        };
        let mut record = crate::audit::AuditEvent::new(
            event_id,
            context.clone(),
            span_id,
            category,
            event,
            // No date-time dependency in this crate, and the repo convention
            // is nanoseconds everywhere internally (see CLAUDE.md), so the
            // timestamp is epoch nanos as a decimal string. It sorts
            // lexically within a fixed width and needs no formatter.
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos())
                .to_string(),
        );
        record.level = level;
        record.mode = self.audit.mode;
        record.duration_ns = duration_ns;
        for (key, value) in fields {
            record = record.with_field(*key, value.clone());
        }
        let _ = sink.emit(record.apply_redaction(&self.audit.redaction));
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
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut done = None;
        self.compile_streaming(request, |chunk| match chunk {
            CompileChunk::Stdout(bytes) => stdout.extend_from_slice(&bytes),
            CompileChunk::Stderr(bytes) => stderr.extend_from_slice(&bytes),
            CompileChunk::Done {
                exit_code,
                cached,
                cache_outcome,
                compile_id,
            } => done = Some((exit_code, cached, cache_outcome, compile_id)),
        })
        .await?;
        let (exit_code, cached, cache_outcome, compile_id) = done.ok_or_else(|| {
            EmbeddedError::Compile("streaming compile completed without a Done event".to_string())
        })?;
        Ok(CompileResponse {
            exit_code,
            stdout,
            stderr,
            cached,
            cache_outcome,
            compile_id,
        })
    }

    async fn compile_inner(&self, request: CompileRequest) -> Result<CompileResponse> {
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
        // Carry the resolved `compile_id` on every event for this compile, so
        // the host's own records correlate by id rather than by timestamp.
        let audit = {
            let mut audit = request.audit.clone();
            audit.compile_id = crate::audit::AuditId::new(compile_id.clone()).ok();
            audit
        };
        let started = std::time::Instant::now();
        self.emit_audit(
            &audit,
            crate::audit::AuditCategory::ZCCACHE_COMPILE,
            crate::audit::AuditEventName::COMPILE_STARTED,
            crate::audit::AuditLevel::Info,
            None,
            &[(
                "compiler",
                serde_json::Value::from(request.compiler.as_path().display().to_string()),
            )],
        );
        let compile_future = self.daemon.compile(EmbeddedCompileRequest {
            compiler: request.compiler.into_path_buf(),
            args: request.args,
            cwd: request.cwd.into_path_buf(),
            env: Some(request.env),
            stdin: request.stdin,
        });
        let outcome = match &self.cancellation {
            Some(token) => {
                let cancelled = token.cancelled();
                tokio::select! {
                    biased;
                    () = cancelled => Err(EmbeddedError::Cancelled),
                    result = compile_future => result.map_err(EmbeddedError::Compile),
                }
            }
            None => compile_future.await.map_err(EmbeddedError::Compile),
        };
        // Every `compile.started` must get a `compile.finished`, including on
        // the cancel and spawn-failure paths. An audit log with dangling
        // starts cannot be used to measure anything, and those are exactly
        // the compiles an operator most wants to find.
        let response = match outcome {
            Ok(response) => response,
            Err(error) => {
                self.emit_audit(
                    &audit,
                    crate::audit::AuditCategory::ZCCACHE_COMPILE,
                    crate::audit::AuditEventName::COMPILE_FINISHED,
                    crate::audit::AuditLevel::Error,
                    Some(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)),
                    &[("error", serde_json::Value::from(error.to_string()))],
                );
                return Err(error);
            }
        };
        let cache_outcome = if response.exit_code != 0 {
            CacheOutcome::Error
        } else if response.cached {
            CacheOutcome::Hit
        } else {
            CacheOutcome::Miss
        };
        let elapsed_ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        // No `cache_key` field: the key is derived inside the engine and is
        // not visible at this boundary. Emitting `cache.lookup` with the key
        // needs plumbing through `EmbeddedCompileRequest` — deliberately left
        // to a follow-up rather than guessed at here.
        match cache_outcome {
            CacheOutcome::Hit => self.emit_audit(
                &audit,
                crate::audit::AuditCategory::ZCCACHE_CACHE_LOOKUP,
                crate::audit::AuditEventName::CACHE_HIT,
                crate::audit::AuditLevel::Info,
                Some(elapsed_ns),
                &[],
            ),
            CacheOutcome::Miss => self.emit_audit(
                &audit,
                crate::audit::AuditCategory::ZCCACHE_CACHE_LOOKUP,
                crate::audit::AuditEventName::CACHE_MISS,
                crate::audit::AuditLevel::Info,
                Some(elapsed_ns),
                &[],
            ),
            // A failed compile is not a cache outcome; `compile.finished`
            // below carries the exit code.
            CacheOutcome::Error => {}
        }
        self.emit_audit(
            &audit,
            crate::audit::AuditCategory::ZCCACHE_COMPILE,
            crate::audit::AuditEventName::COMPILE_FINISHED,
            if response.exit_code == 0 {
                crate::audit::AuditLevel::Info
            } else {
                crate::audit::AuditLevel::Error
            },
            Some(elapsed_ns),
            &[
                ("exit_code", serde_json::Value::from(response.exit_code)),
                ("cached", serde_json::Value::from(response.cached)),
            ],
        );
        Ok(CompileResponse {
            exit_code: response.exit_code,
            stdout: response.stdout.as_ref().clone(),
            stderr: response.stderr.as_ref().clone(),
            cached: response.cached,
            cache_outcome,
            compile_id,
        })
    }

    /// Streaming compile (issue #937). Invokes `on_chunk` as compiler output
    /// arrives, then once with the terminal `Done` event. Per-stream ordering
    /// is preserved; ordering between stdout and stderr follows the pipe drain.
    ///
    /// The callback runs inline and applies backpressure to the bounded drain.
    /// Retained output is capped at 1 MiB per stream by default; set
    /// `ZCCACHE_STREAM_CAPTURE_LIMIT_BYTES` to a positive byte count to change
    /// it. Truncation is explicit and the same marker is cached and replayed.
    pub async fn compile_streaming<F>(&self, request: CompileRequest, mut on_chunk: F) -> Result<()>
    where
        F: FnMut(CompileChunk),
    {
        const CHUNK_BYTES: usize = 64 * 1024;
        let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
        let context = crate::daemon::compile_output::OutputContext::new(sender);
        let compile =
            crate::daemon::compile_output::scope(context.clone(), self.compile_inner(request));
        tokio::pin!(compile);

        let response = loop {
            tokio::select! {
                biased;
                chunk = receiver.recv() => {
                    if let Some(chunk) = chunk {
                        emit_output_chunk(&mut on_chunk, chunk);
                    }
                }
                result = &mut compile => break result?,
            }
        };
        while let Ok(chunk) = receiver.try_recv() {
            emit_output_chunk(&mut on_chunk, chunk);
        }

        if !context.was_live() {
            for chunk in response.stdout.chunks(CHUNK_BYTES) {
                on_chunk(CompileChunk::Stdout(chunk.to_vec()));
            }
            for chunk in response.stderr.chunks(CHUNK_BYTES) {
                on_chunk(CompileChunk::Stderr(chunk.to_vec()));
            }
        }
        on_chunk(CompileChunk::Done {
            exit_code: response.exit_code,
            cached: response.cached,
            cache_outcome: response.cache_outcome,
            compile_id: response.compile_id,
        });
        Ok(())
    }

    /// Return a daemon-compatible stats snapshot.
    pub async fn stats(&self) -> Result<ServiceStats> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(EmbeddedError::ShutDown);
        }
        Ok(ServiceStats::from_snapshot(self.daemon.stats().await))
    }

    /// Run disk maintenance against this service's exact configured cache root.
    ///
    /// [`MaintenanceOwnership::Embedded`] also runs pressure checks every five
    /// minutes and a persisted full-age pass every 24 hours.
    /// [`MaintenanceOwnership::Host`] suppresses that scheduler so the host
    /// calls this method from its own lifecycle loop. Neither form scans
    /// sibling product roots.
    pub async fn maintain_disk(
        &self,
        kind: DiskMaintenanceKind,
    ) -> std::io::Result<DiskMaintenanceReport> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "embedded zccache service is already shut down",
            ));
        }
        let kind = match kind {
            DiskMaintenanceKind::Pressure => InternalMaintenanceKind::Pressure,
            DiskMaintenanceKind::Full => InternalMaintenanceKind::Full,
        };
        self.daemon
            .maintain_disk(kind)
            .await
            .map(DiskMaintenanceReport::from_report)
    }

    /// Flush pending embedded service state to disk.
    ///
    /// A token cancelled before the flush starts returns
    /// [`EmbeddedError::Cancelled`]. Once persistence begins it remains owned
    /// until completion: dropping an awaited `spawn_blocking` join does not
    /// cancel the disk write and could let an older checkpoint race a later
    /// archive.
    async fn flush_internal(&self) -> Result<EmbeddedFlushReport> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(EmbeddedError::ShutDown);
        }
        if let Some(token) = &self.cancellation {
            if token.is_cancelled() {
                return Err(EmbeddedError::Cancelled);
            }
        }
        let report = self.daemon.flush().await;
        // zccache#926: drain pending audit events to disk along with
        // the cache state. Best-effort — a failure to flush the audit
        // sink does not block the embedded service flush from
        // succeeding; it only means the host saw a possibly-empty
        // tail in the JSONL.
        if let Some(sink) = &self.audit_sink {
            let _ = sink.flush().await;
        }
        Ok(report)
    }

    pub async fn flush(&self) -> Result<FlushReport> {
        self.flush_internal().await.map(FlushReport::from_report)
    }

    /// Flush pending embedded state and report every durability phase.
    ///
    /// Persistence workers remain owned until they complete. A non-shutdown
    /// flush can still return an incomplete report if its publication barrier
    /// cannot be acquired before the safety deadline.
    pub async fn flush_detailed(&self) -> Result<DetailedFlushReport> {
        self.flush_internal()
            .await
            .map(DetailedFlushReport::from_report)
    }

    /// Shut down the service and flush relevant persisted state.
    ///
    /// `ShutdownMode::Graceful` waits for the durable audit sink to
    /// drain before returning. `ShutdownMode::Force` does not — the
    /// host signalled "stop now, lost events are acceptable."
    async fn shutdown_internal(self, mode: ShutdownMode) -> Result<EmbeddedFlushReport> {
        if self.shutdown.swap(true, Ordering::AcqRel) {
            return Err(EmbeddedError::ShutDown);
        }
        let report = self.daemon.shutdown().await;
        // zccache#926: shut the audit sink down when going Graceful.
        // Force skips this so the host can exit quickly under SIGINT
        // even if the disk is slow.
        if matches!(mode, ShutdownMode::Graceful) {
            if let Some(sink) = &self.audit_sink {
                let _ = sink.shutdown().await;
            }
        }
        Ok(report)
    }

    pub async fn shutdown(self, mode: ShutdownMode) -> Result<ShutdownReport> {
        let report = self.shutdown_internal(mode).await?;
        Ok(ShutdownReport {
            mode,
            flushed: FlushReport::from_report(report),
        })
    }

    /// Shut down the service and report every durability phase.
    pub async fn shutdown_detailed(self, mode: ShutdownMode) -> Result<DetailedShutdownReport> {
        let report = self.shutdown_internal(mode).await?;
        Ok(DetailedShutdownReport {
            mode,
            flushed: DetailedFlushReport::from_report(report),
        })
    }
}

fn emit_output_chunk<F>(on_chunk: &mut F, chunk: crate::daemon::compile_output::OutputChunk)
where
    F: FnMut(CompileChunk),
{
    match chunk {
        crate::daemon::compile_output::OutputChunk::Stdout(bytes) => {
            on_chunk(CompileChunk::Stdout(bytes));
        }
        crate::daemon::compile_output::OutputChunk::Stderr(bytes) => {
            on_chunk(CompileChunk::Stderr(bytes));
        }
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

#[cfg(test)]
mod flush_report_compatibility_tests {
    use super::FlushReport;

    #[test]
    fn legacy_flush_report_literal_and_exhaustive_pattern_still_compile() {
        let report = FlushReport {
            pending_writes_drained: true,
            artifact_entries: 2,
            metadata_entries: 3,
        };
        let FlushReport {
            pending_writes_drained,
            artifact_entries,
            metadata_entries,
        } = report;
        assert!(pending_writes_drained);
        assert_eq!((artifact_entries, metadata_entries), (2, 3));
    }
}

impl DetailedFlushReport {
    fn from_report(report: EmbeddedFlushReport) -> Self {
        debug_assert_eq!(report.is_complete(), {
            report.pending_writes_drained
                && report.index_writer_drained
                && report.steps.iter().all(|step| {
                    matches!(
                        step.outcome,
                        crate::daemon::server::FlushStepOutcome::Completed
                    )
                })
        });
        Self {
            pending_writes_drained: report.pending_writes_drained,
            index_writer_drained: report.index_writer_drained,
            steps: report
                .steps
                .into_iter()
                .map(|step| FlushStepReport {
                    step: step.step,
                    outcome: match step.outcome {
                        crate::daemon::server::FlushStepOutcome::Completed => {
                            FlushStepOutcome::Completed
                        }
                        crate::daemon::server::FlushStepOutcome::Failed(error) => {
                            FlushStepOutcome::Failed(error)
                        }
                        crate::daemon::server::FlushStepOutcome::TimedOut => {
                            FlushStepOutcome::TimedOut
                        }
                    },
                })
                .collect(),
            artifact_entries: report.artifact_entries,
            metadata_entries: report.metadata_entries,
        }
    }
}

impl DiskMaintenanceReport {
    fn from_report(report: InternalDiskMaintenanceReport) -> Self {
        Self {
            kind: match report.kind {
                InternalMaintenanceKind::Pressure => DiskMaintenanceKind::Pressure,
                InternalMaintenanceKind::Full => DiskMaintenanceKind::Full,
            },
            pressure: match report.pressure {
                InternalMaintenancePressure::None => DiskMaintenancePressure::None,
                InternalMaintenancePressure::Soft => DiskMaintenancePressure::Soft,
                InternalMaintenancePressure::Hard => DiskMaintenancePressure::Hard,
            },
            budget_bytes: report.budget_bytes,
            usage_before_bytes: report.usage_before_bytes,
            usage_after_bytes: report.usage_after_bytes,
            bytes_reclaimed: report.bytes_reclaimed,
            artifacts_removed: report.artifacts_removed,
            expired_artifacts_removed: report.expired_artifacts_removed,
            pending_write_bytes: report.pending_write_bytes,
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
#[path = "embedded/tests.rs"]
mod tests;

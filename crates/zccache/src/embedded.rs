//! First-class in-process zccache service API.
//!
//! This module exposes the embedded service contract used by host daemons that
//! already own a Tokio runtime. The service reuses the daemon compile/session
//! machinery directly and does not bind or listen on zccache IPC endpoints.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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
}

/// Opaque in-process zccache service handle.
#[derive(Clone)]
pub struct ZccacheService {
    daemon: Arc<EmbeddedDaemon>,
    shutdown: Arc<AtomicBool>,
}

/// Configuration for [`ZccacheService::start`].
#[derive(Debug, Clone)]
pub struct ZccacheConfig {
    pub host: HostIdentity,
    pub cache_root: PathBuf,
    pub audit: AuditConfig,
    pub limits: ServiceLimits,
    pub runtime: RuntimeHooks,
}

/// Host identity used to namespace and diagnose an embedded service instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostIdentity {
    pub product: String,
    pub instance_id: String,
    pub workspace_id: String,
}

/// Runtime integration hooks reserved for host-owned Tokio runtimes.
#[derive(Debug, Clone, Default)]
pub struct RuntimeHooks {
    pub service_name: Option<String>,
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
    pub compiler: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
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
    pub cache_root: PathBuf,
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
    pub async fn start(config: ZccacheConfig) -> Result<Self> {
        let endpoint = embedded_endpoint(&config.host);
        let cache_root = crate::core::config::effective_cache_root_from_top_level(
            &crate::core::NormalizedPath::new(config.cache_root),
        );
        let daemon = EmbeddedDaemon::start(endpoint, cache_root)
            .await
            .map_err(|err| EmbeddedError::Start(err.to_string()))?;
        Ok(Self {
            daemon: Arc::new(daemon),
            shutdown: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Compile using the embedded daemon engine.
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
        let response = self
            .daemon
            .compile(EmbeddedCompileRequest {
                compiler: request.compiler,
                args: request.args,
                cwd: request.cwd,
                env: Some(request.env),
                stdin: request.stdin,
            })
            .await
            .map_err(EmbeddedError::Compile)?;
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
    pub async fn flush(&self) -> Result<FlushReport> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(EmbeddedError::ShutDown);
        }
        Ok(FlushReport::from_report(self.daemon.flush().await))
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
            cache_root: status.cache_dir.into_path_buf(),
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

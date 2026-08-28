//! Additive host integration and compile-control plumbing for embedded mode.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use super::{
    EmbeddedError, HostAdmissionClassifier, HostIdentity, MaintenanceOwnership, MaintenancePolicy,
    Result, ZccacheConfig, ZccacheService, ZccacheStartOptions,
};
use crate::daemon::server::EmbeddedDaemon;

/// Receives structured events emitted by an embedded service.
///
/// The callback runs inline on the compile task and should return quickly.
/// Panics are contained and do not fail the compile. Events are redacted using
/// the service's audit configuration before this callback runs. Registering a
/// host sink is explicit, so it receives events even when file audit is off.
pub trait EmbeddedEventSink: Send + Sync + 'static {
    fn emit(&self, event: &crate::audit::AuditEvent);
}

impl<F> EmbeddedEventSink for F
where
    F: Fn(&crate::audit::AuditEvent) + Send + Sync + 'static,
{
    fn emit(&self, event: &crate::audit::AuditEvent) {
        self(event);
    }
}

impl ZccacheService {
    /// Start an embedded service with a host-owned structured event sink.
    ///
    /// This additive constructor keeps [`ZccacheConfig`] and
    /// [`ZccacheStartOptions`] struct literals source-compatible. The sink
    /// receives redacted compile events regardless of whether file audit
    /// output is enabled.
    pub async fn start_with_event_sink(
        config: ZccacheConfig,
        event_sink: Arc<dyn EmbeddedEventSink>,
    ) -> Result<Self> {
        Self::start_internal(
            config,
            ZccacheStartOptions::default(),
            Some(event_sink),
            None,
        )
        .await
    }

    /// Start with both additive service options and a host event sink.
    pub async fn start_with_options_and_event_sink(
        config: ZccacheConfig,
        options: ZccacheStartOptions,
        event_sink: Arc<dyn EmbeddedEventSink>,
    ) -> Result<Self> {
        Self::start_internal(config, options, Some(event_sink), None).await
    }

    pub(super) async fn start_internal(
        config: ZccacheConfig,
        options: ZccacheStartOptions,
        event_sink: Option<Arc<dyn EmbeddedEventSink>>,
        host_admission_classifier: Option<Arc<dyn HostAdmissionClassifier>>,
    ) -> Result<Self> {
        let compile_permits = match config.limits.max_parallel_compiles {
            Some(0) => {
                return Err(EmbeddedError::Start(
                    "max_parallel_compiles must be greater than zero".to_string(),
                ));
            }
            Some(limit) if limit > Semaphore::MAX_PERMITS => {
                return Err(EmbeddedError::Start(format!(
                    "max_parallel_compiles must not exceed {}",
                    Semaphore::MAX_PERMITS
                )));
            }
            Some(limit) => Some(Arc::new(Semaphore::new(limit))),
            None => None,
        };
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
            host_admission_classifier,
        )
        .await
        .map_err(|err| EmbeddedError::Start(err.to_string()))?;
        let host_inflight_guard = config
            .limits
            .host_in_flight
            .map(crate::daemon::process::register_host_in_flight_counter)
            .map(Arc::new);
        let audit_sink =
            crate::audit_writer::AuditSink::start(&config.audit, config.runtime.handle.clone())
                .map_err(|err| EmbeddedError::Start(err.to_string()))?
                .map(Arc::new);
        Ok(Self {
            daemon: Arc::new(daemon),
            shutdown: Arc::new(AtomicBool::new(false)),
            host_cancellation: config.cancellation,
            force_cancellation: CancellationToken::new(),
            compile_permits,
            _host_inflight_guard: host_inflight_guard,
            audit_sink,
            event_sink,
            audit: Arc::new(config.audit),
        })
    }

    pub(super) async fn acquire_compile_permit(&self) -> Result<Option<OwnedSemaphorePermit>> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(EmbeddedError::ShutDown);
        }
        if self.force_cancellation.is_cancelled()
            || self
                .host_cancellation
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(EmbeddedError::Cancelled);
        }
        let Some(semaphore) = &self.compile_permits else {
            return Ok(None);
        };
        let permit = Arc::clone(semaphore).acquire_owned();
        let permit = match &self.host_cancellation {
            Some(token) => {
                tokio::select! {
                    biased;
                    () = self.force_cancellation.cancelled() => return Err(EmbeddedError::Cancelled),
                    () = token.cancelled() => return Err(EmbeddedError::Cancelled),
                    permit = permit => permit,
                }
            }
            None => {
                tokio::select! {
                    biased;
                    () = self.force_cancellation.cancelled() => return Err(EmbeddedError::Cancelled),
                    permit = permit => permit,
                }
            }
        }
        .map_err(|_| EmbeddedError::ShutDown)?;
        Ok(Some(permit))
    }

    pub(super) async fn await_compile<T, F>(&self, compile: F) -> Result<T>
    where
        F: std::future::Future<Output = std::result::Result<T, String>>,
    {
        match &self.host_cancellation {
            Some(token) => {
                tokio::select! {
                    biased;
                    () = self.force_cancellation.cancelled() => Err(EmbeddedError::Cancelled),
                    () = token.cancelled() => Err(EmbeddedError::Cancelled),
                    result = compile => result.map_err(EmbeddedError::Compile),
                }
            }
            None => {
                tokio::select! {
                    biased;
                    () = self.force_cancellation.cancelled() => Err(EmbeddedError::Cancelled),
                    result = compile => result.map_err(EmbeddedError::Compile),
                }
            }
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

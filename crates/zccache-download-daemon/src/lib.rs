#![allow(clippy::missing_errors_doc)]

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use dashmap::DashMap;
use tokio::sync::{watch, Notify, RwLock};
use tokio_util::sync::CancellationToken;
use zccache_monocrate::core::NormalizedPath;
use zccache_download::{
    percentage, stable_download_id, DownloadDaemonStatus, DownloadOptions, DownloadPhase,
    DownloadStatus,
};
use zccache_download_protocol::{Request, Response};

#[derive(Clone)]
struct FileLogger {
    file: Arc<Mutex<std::fs::File>>,
}

impl FileLogger {
    fn new(path: &std::path::Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self {
            file: Arc::new(Mutex::new(file)),
        })
    }

    fn log(&self, message: &str) {
        if let Ok(mut file) = self.file.lock() {
            let _ = writeln!(file, "{message}");
        }
    }
}

struct DownloadJob {
    id: String,
    url: String,
    destination: NormalizedPath,
    metadata_dir: NormalizedPath,
    status: RwLock<DownloadStatus>,
    updates: watch::Sender<u64>,
    active_clients: AtomicUsize,
    cancel_token: CancellationToken,
    cleanup_pending: AtomicBool,
}

impl DownloadJob {}

struct SharedState {
    endpoint: String,
    jobs: DashMap<String, Arc<DownloadJob>>,
    shutdown: Arc<Notify>,
    start_time: Instant,
    logger: FileLogger,
    next_client_id: AtomicU64,
}

pub struct DownloadDaemon {
    listener: zccache_ipc::IpcListener,
    state: Arc<SharedState>,
}

impl DownloadDaemon {
    pub fn bind(endpoint: &str) -> Result<Self, zccache_ipc::IpcError> {
        let listener = zccache_ipc::IpcListener::bind(endpoint)?;
        let log_path = zccache_monocrate::core::config::log_dir().join("download-daemon.log");
        let logger = FileLogger::new(&log_path)
            .map_err(|e| zccache_ipc::IpcError::Io(std::io::Error::other(e.to_string())))?;
        Ok(Self {
            listener,
            state: Arc::new(SharedState {
                endpoint: endpoint.to_string(),
                jobs: DashMap::new(),
                shutdown: Arc::new(Notify::new()),
                start_time: Instant::now(),
                logger,
                next_client_id: AtomicU64::new(1),
            }),
        })
    }

    #[must_use]
    pub fn shutdown_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.state.shutdown)
    }

    pub async fn run(&mut self) -> Result<(), zccache_ipc::IpcError> {
        loop {
            tokio::select! {
                _ = self.state.shutdown.notified() => {
                    self.state.logger.log("download daemon shutdown requested");
                    break;
                }
                accepted = self.listener.accept() => {
                    let conn = accepted?;
                    let state = Arc::clone(&self.state);
                    tokio::spawn(async move {
                        if let Err(err) = handle_connection(state, conn).await {
                            tracing::warn!("download connection error: {err}");
                        }
                    });
                }
            }
        }
        Ok(())
    }
}

async fn handle_connection(
    state: Arc<SharedState>,
    mut conn: zccache_ipc::IpcConnection,
) -> Result<(), String> {
    let client_id = state.next_client_id.fetch_add(1, Ordering::Relaxed);
    let mut attached_job_id: Option<String> = None;

    loop {
        let request = match conn.recv::<Request>().await {
            Ok(Some(req)) => req,
            Ok(None) => {
                if let Some(job_id) = attached_job_id.take() {
                    detach_client(&state, &job_id).await;
                }
                return Ok(());
            }
            Err(err) => {
                if let Some(job_id) = attached_job_id.take() {
                    detach_client(&state, &job_id).await;
                }
                return Err(err.to_string());
            }
        };

        match request {
            Request::Ping => {
                conn.send(&Response::Pong)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Request::Status => {
                conn.send(&Response::Status(daemon_status(&state)))
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Request::Shutdown => {
                state.shutdown.notify_one();
                conn.send(&Response::ShuttingDown)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Request::DownloadAttach {
                url,
                destination,
                options,
            } => {
                if attached_job_id.is_some() {
                    conn.send(&Response::Error {
                        message: "connection already attached to a download".to_string(),
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                    continue;
                }

                match attach_job(&state, &url, destination, options).await {
                    Ok((job, initiator)) => {
                        attached_job_id = Some(job.id.clone());
                        let status = job.status.read().await.clone();
                        state.logger.log(&format!(
                            "client {client_id} attached to {} initiator={initiator}",
                            job.id
                        ));
                        conn.send(&Response::DownloadAttached {
                            download_id: job.id.clone(),
                            initiator,
                            status,
                        })
                        .await
                        .map_err(|e| e.to_string())?;
                    }
                    Err(message) => {
                        conn.send(&Response::Error { message })
                            .await
                            .map_err(|e| e.to_string())?;
                    }
                }
            }
            Request::DownloadStatus => {
                let Some(job_id) = attached_job_id.as_ref() else {
                    conn.send(&Response::Error {
                        message: "connection is not attached to a download".to_string(),
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                    continue;
                };
                match state.jobs.get(job_id) {
                    Some(job) => {
                        let status = job.status.read().await.clone();
                        conn.send(&Response::DownloadStatusResult { status })
                            .await
                            .map_err(|e| e.to_string())?;
                    }
                    None => {
                        conn.send(&Response::Error {
                            message: "download no longer exists".to_string(),
                        })
                        .await
                        .map_err(|e| e.to_string())?;
                    }
                }
            }
            Request::DownloadWait { timeout_ms } => {
                let Some(job_id) = attached_job_id.as_ref() else {
                    conn.send(&Response::Error {
                        message: "connection is not attached to a download".to_string(),
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                    continue;
                };
                let Some(job) = state.jobs.get(job_id).map(|j| Arc::clone(j.value())) else {
                    conn.send(&Response::Error {
                        message: "download no longer exists".to_string(),
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                    continue;
                };
                let current = job.status.read().await.clone();
                if is_terminal(&current) {
                    send_terminal(&mut conn, current).await?;
                    continue;
                }

                let mut rx = job.updates.subscribe();
                let wait_result = if let Some(timeout_ms) = timeout_ms {
                    tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), rx.changed())
                        .await
                        .ok()
                } else {
                    Some(rx.changed().await)
                };
                let status = job.status.read().await.clone();
                match wait_result {
                    Some(Ok(())) => {
                        if is_terminal(&status) {
                            send_terminal(&mut conn, status).await?;
                        } else {
                            conn.send(&Response::DownloadStatusResult { status })
                                .await
                                .map_err(|e| e.to_string())?;
                        }
                    }
                    Some(Err(_)) | None => {
                        if is_terminal(&status) {
                            send_terminal(&mut conn, status).await?;
                        } else {
                            conn.send(&Response::DownloadStatusResult { status })
                                .await
                                .map_err(|e| e.to_string())?;
                        }
                    }
                }
            }
            Request::DownloadCancel => {
                if let Some(job_id) = attached_job_id.take() {
                    let status = cancel_client(&state, &job_id).await;
                    conn.send(&Response::DownloadCancelled { status })
                        .await
                        .map_err(|e| e.to_string())?;
                } else {
                    conn.send(&Response::Error {
                        message: "connection is not attached to a download".to_string(),
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                }
            }
        }
    }
}

async fn send_terminal(
    conn: &mut zccache_ipc::IpcConnection,
    status: DownloadStatus,
) -> Result<(), String> {
    match status.phase {
        DownloadPhase::Completed => conn
            .send(&Response::DownloadFinished { status })
            .await
            .map_err(|e| e.to_string()),
        DownloadPhase::Cancelled | DownloadPhase::Failed => conn
            .send(&Response::DownloadCancelled { status })
            .await
            .map_err(|e| e.to_string()),
        _ => conn
            .send(&Response::DownloadStatusResult { status })
            .await
            .map_err(|e| e.to_string()),
    }
}

fn daemon_status(state: &SharedState) -> DownloadDaemonStatus {
    let connected_clients = state
        .jobs
        .iter()
        .map(|entry| entry.active_clients.load(Ordering::Relaxed) as u64)
        .sum();
    DownloadDaemonStatus {
        version: zccache_monocrate::core::VERSION.to_string(),
        active_downloads: state.jobs.len() as u64,
        connected_clients,
        uptime_secs: state.start_time.elapsed().as_secs(),
        endpoint: state.endpoint.clone(),
    }
}

fn is_terminal(status: &DownloadStatus) -> bool {
    matches!(
        status.phase,
        DownloadPhase::Completed | DownloadPhase::Cancelled | DownloadPhase::Failed
    )
}

async fn attach_job(
    state: &Arc<SharedState>,
    url: &str,
    destination: NormalizedPath,
    options: DownloadOptions,
) -> Result<(Arc<DownloadJob>, bool), String> {
    let download_id = stable_download_id(&destination);
    if let Some(existing) = state.jobs.get(&download_id) {
        let existing = Arc::clone(existing.value());
        if existing.url != url {
            return Err(format!(
                "destination {} is already downloading from {}",
                destination.display(),
                existing.url
            ));
        }
        existing.active_clients.fetch_add(1, Ordering::Relaxed);
        refresh_client_count(&existing).await;
        return Ok((existing, false));
    }

    let metadata_dir = zccache_monocrate::core::config::default_cache_dir()
        .join("downloads")
        .join(&download_id);
    let initial_status = if destination.exists() && !options.force {
        let size = std::fs::metadata(&destination)
            .map(|m| m.len())
            .unwrap_or(0);
        DownloadStatus {
            phase: DownloadPhase::Completed,
            total_bytes: Some(size),
            downloaded_bytes: size,
            percentage: Some(100.0),
            active_clients: 1,
            destination: destination.clone(),
            source_url: url.to_string(),
            error: None,
        }
    } else {
        DownloadStatus {
            phase: DownloadPhase::Pending,
            total_bytes: None,
            downloaded_bytes: 0,
            percentage: None,
            active_clients: 1,
            destination: destination.clone(),
            source_url: url.to_string(),
            error: None,
        }
    };
    let (tx, _rx) = watch::channel(0u64);
    let job = Arc::new(DownloadJob {
        id: download_id.clone(),
        url: url.to_string(),
        destination: destination.clone(),
        metadata_dir,
        status: RwLock::new(initial_status),
        updates: tx,
        active_clients: AtomicUsize::new(1),
        cancel_token: CancellationToken::new(),
        cleanup_pending: AtomicBool::new(false),
    });

    match state.jobs.entry(download_id.clone()) {
        dashmap::mapref::entry::Entry::Occupied(entry) => {
            let existing = Arc::clone(entry.get());
            existing.active_clients.fetch_add(1, Ordering::Relaxed);
            refresh_client_count(&existing).await;
            Ok((existing, false))
        }
        dashmap::mapref::entry::Entry::Vacant(entry) => {
            entry.insert(Arc::clone(&job));
            state.logger.log(&format!(
                "download created id={} destination={}",
                job.id,
                destination.display()
            ));
            let should_spawn = {
                let status = job.status.read().await.clone();
                !is_terminal(&status)
            };
            if should_spawn {
                spawn_download_worker(Arc::clone(state), Arc::clone(&job), options);
            }
            Ok((job, true))
        }
    }
}

fn spawn_download_worker(state: Arc<SharedState>, job: Arc<DownloadJob>, options: DownloadOptions) {
    tokio::spawn(async move {
        let url = job.url.clone();
        let destination = PathBuf::from(job.destination.as_path());
        let metadata_dir = PathBuf::from(job.metadata_dir.as_path());
        let progress_job = Arc::clone(&job);
        let progress = Arc::new(
            move |downloaded: u64, total: Option<u64>, phase: DownloadPhase| {
                let progress_job = Arc::clone(&progress_job);
                tokio::spawn(async move {
                    let mut status = progress_job.status.write().await;
                    status.phase = phase;
                    status.total_bytes = total;
                    status.downloaded_bytes = downloaded;
                    status.percentage = percentage(downloaded, total);
                    status.active_clients =
                        progress_job.active_clients.load(Ordering::Relaxed) as u32;
                    let _ = progress_job.updates.send(downloaded);
                });
            },
        );

        let result = zccache_download::download_to_path(
            &url,
            &destination,
            &metadata_dir,
            &options,
            progress,
            job.cancel_token.clone(),
        )
        .await;

        let mut status = job.status.write().await;
        match result {
            Ok(total) => {
                let total = total.unwrap_or(status.downloaded_bytes);
                status.phase = DownloadPhase::Completed;
                status.total_bytes = Some(total);
                status.downloaded_bytes = total;
                status.percentage = Some(100.0);
                status.error = None;
                state
                    .logger
                    .log(&format!("download completed id={} bytes={total}", job.id));
            }
            Err(zccache_download::DownloadError::Cancelled) => {
                status.phase = DownloadPhase::Cancelled;
                status.error = None;
                state
                    .logger
                    .log(&format!("download cancelled id={}", job.id));
            }
            Err(err) => {
                status.phase = DownloadPhase::Failed;
                status.error = Some(err.to_string());
                state
                    .logger
                    .log(&format!("download failed id={} error={err}", job.id));
            }
        }
        status.active_clients = job.active_clients.load(Ordering::Relaxed) as u32;
        let _ = job.updates.send(status.downloaded_bytes);
        drop(status);

        if job.active_clients.load(Ordering::Relaxed) == 0 {
            state.jobs.remove(&job.id);
        }
    });
}

async fn refresh_client_count(job: &Arc<DownloadJob>) {
    let mut status = job.status.write().await;
    status.active_clients = job.active_clients.load(Ordering::Relaxed) as u32;
    let _ = job.updates.send(status.downloaded_bytes);
}

async fn cancel_client(state: &Arc<SharedState>, job_id: &str) -> DownloadStatus {
    detach_client(state, job_id).await;
    if let Some(job) = state
        .jobs
        .get(job_id)
        .map(|entry| Arc::clone(entry.value()))
    {
        job.status.read().await.clone()
    } else {
        DownloadStatus {
            phase: DownloadPhase::Cancelled,
            total_bytes: None,
            downloaded_bytes: 0,
            percentage: None,
            active_clients: 0,
            destination: NormalizedPath::from(""),
            source_url: String::new(),
            error: None,
        }
    }
}

async fn detach_client(state: &Arc<SharedState>, job_id: &str) {
    let Some(job) = state
        .jobs
        .get(job_id)
        .map(|entry| Arc::clone(entry.value()))
    else {
        return;
    };
    let prev = job.active_clients.fetch_sub(1, Ordering::Relaxed);
    let new_count = prev.saturating_sub(1);
    let downloaded_bytes = {
        let mut status = job.status.write().await;
        status.active_clients = new_count as u32;
        status.downloaded_bytes
    };
    let _ = job.updates.send(downloaded_bytes);
    if new_count == 0 {
        job.cleanup_pending.store(true, Ordering::Relaxed);
        let status = job.status.read().await.clone();
        if !is_terminal(&status) {
            state.logger.log(&format!("download abandoned id={job_id}"));
            job.cancel_token.cancel();
        } else {
            state.jobs.remove(job_id);
        }
    }
}

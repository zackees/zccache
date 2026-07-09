#![allow(clippy::missing_errors_doc)]

mod artifact;

use std::path::Path;

use crate::core::NormalizedPath;
use crate::download::{
    canonical_destination, DownloadAttachResult, DownloadDaemonStatus, DownloadOptions,
    DownloadStatus,
};
use crate::download_protocol::{Request, Response};

pub use artifact::{
    ArchiveFormat, DownloadSource, FetchRequest, FetchResult, FetchState, FetchStateKind,
    FetchStatus, WaitMode,
};

#[cfg(unix)]
type ClientConn = crate::ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = crate::ipc::IpcClientConnection;

pub use crate::download_protocol::daemon_mgmt::{
    default_endpoint, lock_file_path, read_lock_file_pid, remove_lock_file, write_lock_file,
};

pub fn check_running_daemon() -> Option<u32> {
    let pid = read_lock_file_pid()?;
    // Verify the PID actually points at the download daemon. A bare
    // is_process_alive check would falsely accept a recycled PID inherited
    // from a restored CI cache (see issue #132).
    if crate::ipc::verify_pid_exe_stem(pid, "zccache-download-daemon") {
        Some(pid)
    } else {
        remove_lock_file();
        #[cfg(unix)]
        {
            let _ = std::fs::remove_file(default_endpoint());
        }
        None
    }
}

#[cfg(unix)]
async fn connect_client(endpoint: &str) -> Result<ClientConn, crate::ipc::IpcError> {
    crate::ipc::connect(endpoint).await
}

#[cfg(windows)]
async fn connect_client(endpoint: &str) -> Result<ClientConn, crate::ipc::IpcError> {
    crate::ipc::connect(endpoint).await
}

fn resolve_endpoint(explicit: Option<&str>) -> String {
    explicit
        .map(ToOwned::to_owned)
        .or_else(|| std::env::var("ZCCACHE_DOWNLOAD_ENDPOINT").ok())
        .unwrap_or_else(default_endpoint)
}

fn run_async<T>(future: impl std::future::Future<Output = Result<T, String>>) -> Result<T, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to create tokio runtime: {e}"))?
        .block_on(future)
}

fn find_daemon_binary() -> Option<NormalizedPath> {
    let name = if cfg!(windows) {
        "zccache-download-daemon.exe"
    } else {
        "zccache-download-daemon"
    };

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(name);
            if candidate.exists() {
                return Some(candidate.into());
            }
        }
    }

    which_on_path(name)
}

fn which_on_path(name: &str) -> Option<NormalizedPath> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.into());
        }
        #[cfg(windows)]
        if Path::new(name).extension().is_none() {
            let with_exe = dir.join(format!("{name}.exe"));
            if with_exe.is_file() {
                return Some(with_exe.into());
            }
        }
    }
    None
}

fn spawn_daemon(bin: &Path, endpoint: &str) -> Result<(), String> {
    // Issue #982: backstop for the host no-spawn guard (see
    // `core::config::NO_SPAWN_ENV`) — the download daemon is covered
    // by the same contract as the compile daemon.
    if crate::core::config::daemon_spawn_disabled() {
        return Err(crate::core::config::no_spawn_error(
            "zccache-download-daemon",
        ));
    }
    let mut cmd = std::process::Command::new(bin);
    cmd.args(["--foreground", "--endpoint", endpoint]);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn()
        .map_err(|e| format!("failed to spawn download daemon: {e}"))?;
    Ok(())
}

async fn ensure_daemon(endpoint: &str) -> Result<(), String> {
    if connect_client(endpoint).await.is_ok() {
        return Ok(());
    }
    if let Some(pid) = check_running_daemon() {
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if connect_client(endpoint).await.is_ok() {
                return Ok(());
            }
        }
        return Err(format!(
            "download daemon process {pid} exists but is not accepting connections"
        ));
    }
    // Issue #982: refuse before binary resolution so the guard's message
    // wins over "cannot find zccache-download-daemon binary".
    if crate::core::config::daemon_spawn_disabled() {
        return Err(crate::core::config::no_spawn_error(
            "zccache-download-daemon",
        ));
    }
    let bin = find_daemon_binary().ok_or("cannot find zccache-download-daemon binary")?;
    spawn_daemon(&bin, endpoint)?;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if connect_client(endpoint).await.is_ok() {
            return Ok(());
        }
    }
    Err("download daemon started but did not accept connections after 10s".to_string())
}

#[derive(Clone)]
pub struct DownloadClient {
    endpoint: Option<String>,
}

impl DownloadClient {
    #[must_use]
    pub fn new(endpoint: Option<String>) -> Self {
        Self { endpoint }
    }

    #[must_use]
    pub fn resolved_endpoint(&self) -> String {
        resolve_endpoint(self.endpoint.as_deref())
    }

    pub fn start_daemon(&self) -> Result<(), String> {
        let client = self.clone();
        run_async(async move { client.start_daemon_async().await })
    }

    pub fn stop_daemon(&self) -> Result<bool, String> {
        let client = self.clone();
        run_async(async move { client.stop_daemon_async().await })
    }

    pub fn daemon_status(&self) -> Result<DownloadDaemonStatus, String> {
        let client = self.clone();
        run_async(async move { client.daemon_status_async().await })
    }

    pub async fn start_daemon_async(&self) -> Result<(), String> {
        let endpoint = self.resolved_endpoint();
        ensure_daemon(&endpoint).await
    }

    pub async fn stop_daemon_async(&self) -> Result<bool, String> {
        let endpoint = self.resolved_endpoint();
        let mut conn = match connect_client(&endpoint).await {
            Ok(conn) => conn,
            Err(_) => return Ok(false),
        };
        conn.send(&Request::Shutdown)
            .await
            .map_err(|e| format!("failed to send shutdown to download daemon: {e}"))?;
        match conn.recv::<Response>().await {
            Ok(Some(Response::ShuttingDown)) => Ok(true),
            Ok(Some(Response::Error { message })) => Err(message),
            Ok(Some(other)) => Err(format!("unexpected response: {other:?}")),
            Ok(None) => Err("download daemon closed connection unexpectedly".to_string()),
            Err(e) => Err(format!("broken connection to download daemon: {e}")),
        }
    }

    pub async fn daemon_status_async(&self) -> Result<DownloadDaemonStatus, String> {
        let endpoint = self.resolved_endpoint();
        let mut conn = connect_client(&endpoint)
            .await
            .map_err(|e| format!("download daemon not running at {endpoint}: {e}"))?;
        conn.send(&Request::Status)
            .await
            .map_err(|e| format!("failed to query download daemon: {e}"))?;
        match conn.recv::<Response>().await {
            Ok(Some(Response::Status(status))) => Ok(status),
            Ok(Some(Response::Error { message })) => Err(message),
            Ok(Some(other)) => Err(format!("unexpected response: {other:?}")),
            Ok(None) => Err("download daemon closed connection unexpectedly".to_string()),
            Err(e) => Err(format!("broken connection to download daemon: {e}")),
        }
    }

    pub fn download(
        &self,
        url: &str,
        destination: &Path,
        options: DownloadOptions,
    ) -> Result<DownloadHandle, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("failed to create runtime: {e}"))?;
        let async_handle = runtime.block_on(self.download_async(url, destination, options))?;
        Ok(DownloadHandle {
            runtime,
            conn: async_handle.conn,
            initiator: async_handle.initiator,
            download_id: async_handle.download_id,
        })
    }

    pub async fn download_async(
        &self,
        url: &str,
        destination: &Path,
        options: DownloadOptions,
    ) -> Result<AsyncDownloadHandle, String> {
        let endpoint = self.resolved_endpoint();
        let url = url.to_string();
        let destination = canonical_destination(destination).map_err(|e| e.to_string())?;
        ensure_daemon(&endpoint).await?;
        let mut conn = connect_client(&endpoint)
            .await
            .map_err(|e| format!("cannot connect to download daemon at {endpoint}: {e}"))?;
        conn.send(&Request::DownloadAttach {
            url: url.clone(),
            destination,
            options,
        })
        .await
        .map_err(|e| format!("failed to send attach request: {e}"))?;
        match conn.recv::<Response>().await {
            Ok(Some(Response::DownloadAttached {
                download_id,
                initiator,
                status: _,
            })) => Ok(AsyncDownloadHandle {
                conn,
                initiator,
                download_id,
            }),
            Ok(Some(Response::Error { message })) => Err(message),
            Ok(Some(other)) => Err(format!("unexpected response: {other:?}")),
            Ok(None) => Err("download daemon closed connection unexpectedly".to_string()),
            Err(e) => Err(format!("broken connection to download daemon: {e}")),
        }
    }
}

pub struct AsyncDownloadHandle {
    conn: ClientConn,
    initiator: bool,
    download_id: String,
}

impl AsyncDownloadHandle {
    #[must_use]
    pub fn initiator(&self) -> bool {
        self.initiator
    }

    #[must_use]
    pub fn download_id(&self) -> &str {
        &self.download_id
    }

    pub async fn status(&mut self) -> Result<DownloadStatus, String> {
        send_download_command(&mut self.conn, Request::DownloadStatus).await
    }

    pub async fn wait(&mut self, timeout_ms: Option<u64>) -> Result<DownloadStatus, String> {
        send_download_command(&mut self.conn, Request::DownloadWait { timeout_ms }).await
    }

    pub async fn cancel(&mut self) -> Result<DownloadStatus, String> {
        send_download_command(&mut self.conn, Request::DownloadCancel).await
    }

    pub fn close(self) -> Result<(), String> {
        Ok(())
    }
}

pub struct DownloadHandle {
    runtime: tokio::runtime::Runtime,
    conn: ClientConn,
    initiator: bool,
    download_id: String,
}

impl DownloadHandle {
    #[must_use]
    pub fn initiator(&self) -> bool {
        self.initiator
    }

    #[must_use]
    pub fn download_id(&self) -> &str {
        &self.download_id
    }

    pub fn status(&mut self) -> Result<DownloadStatus, String> {
        self.runtime.block_on(send_download_command(
            &mut self.conn,
            Request::DownloadStatus,
        ))
    }

    pub fn wait(&mut self, timeout_ms: Option<u64>) -> Result<DownloadStatus, String> {
        self.runtime.block_on(send_download_command(
            &mut self.conn,
            Request::DownloadWait { timeout_ms },
        ))
    }

    pub fn cancel(&mut self) -> Result<DownloadStatus, String> {
        self.runtime.block_on(send_download_command(
            &mut self.conn,
            Request::DownloadCancel,
        ))
    }

    pub fn close(self) -> Result<(), String> {
        Ok(())
    }
}

async fn send_download_command(
    conn: &mut ClientConn,
    request: Request,
) -> Result<DownloadStatus, String> {
    let action = match &request {
        Request::DownloadStatus => "status",
        Request::DownloadWait { .. } => "wait",
        Request::DownloadCancel => "cancel",
        _ => "download",
    };
    conn.send(&request)
        .await
        .map_err(|e| format!("failed to send {action} request: {e}"))?;
    match conn.recv::<Response>().await {
        Ok(Some(Response::DownloadStatusResult { status })) => Ok(status),
        Ok(Some(Response::DownloadFinished { status })) => Ok(status),
        Ok(Some(Response::DownloadCancelled { status })) => Ok(status),
        Ok(Some(Response::Error { message })) => Err(message),
        Ok(Some(other)) => Err(format!("unexpected response: {other:?}")),
        Ok(None) => Err("download daemon closed connection unexpectedly".to_string()),
        Err(e) => Err(format!("broken connection to download daemon: {e}")),
    }
}

pub fn is_terminal(status: &DownloadStatus) -> bool {
    matches!(
        status.phase,
        crate::download::DownloadPhase::Completed
            | crate::download::DownloadPhase::Cancelled
            | crate::download::DownloadPhase::Failed
    )
}

pub fn coerce_attach_result(
    download_id: String,
    initiator: bool,
    status: DownloadStatus,
) -> DownloadAttachResult {
    DownloadAttachResult {
        download_id,
        initiator,
        status,
    }
}

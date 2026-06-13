//! BOUNDED EMPIRICAL SPIKE (Unix-only): does the live socket handed back by
//! `running_process::broker::adopt::AsyncBrokerSession::into_backend_io()`
//! carry zccache's FrameV1 application protocol through to a real zccache
//! `DaemonServer`, so a `Ping` gets a `Pong`?
//!
//! Arrangement (closest faithful to a real broker fronting the real daemon):
//!   * A real zccache `DaemonServer` binds its own Unix socket and serves the
//!     normal daemon wire (FrameV1 included), exactly as in production.
//!   * A real running-process broker (`HelloHandler` + `handle_hello_connection`,
//!     copied verbatim from running-process 4.4.0
//!     `tests/broker/into_backend_io.rs`) registers that daemon socket as the
//!     negotiated `backend_pipe`. This is the `BrokerNegotiated` route: the
//!     broker tells the client the backend endpoint, the client dials it, and
//!     `into_backend_io()` hands back THAT live socket. The broker never itself
//!     connects to the daemon, so the adopted connection is the client's first
//!     and only connection to the daemon.
//!   * Client: `AsyncBrokerSession::adopt(...)` -> `into_backend_io()` ->
//!     `OwnedFd` -> `tokio::net::UnixStream` -> `IpcConnection::from_unix_stream`
//!     -> FrameV1 `Ping` -> assert `Pong`.
//!
//! We do NOT use the lower-level `serve_registered_backend(BrokerServeConfig)`
//! serve loop because it requires an on-disk `<service>.servicedef` file and
//! probes the backend identity first. `HelloHandler::with_backend` carries the
//! `ServiceDefinition` inline and skips the probe, which is the minimal faithful
//! broker that fronts an external backend socket.

#![cfg(unix)]

use std::io;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use interprocess::local_socket::traits::Listener as _;
use interprocess::local_socket::ListenerOptions;
use running_process::broker::adopt::{AsyncBrokerSession, OwnedConnectRequest};
use running_process::broker::client::BackendConnectionRoute;
use running_process::broker::protocol::{BrokerIsolation, ServiceDefinition};
use running_process::broker::server::{
    handle_hello_connection, local_socket_name, HelloHandler, PeerIdentity, RegisteredBackend,
};

use zccache::daemon::DaemonServer;
use zccache::protocol::wire_prost::WireFormat;
use zccache::protocol::{Request, Response};

const SPIKE_SERVICE: &str = "zccache";

// ── socket_common helpers, copied from running-process 4.4.0 tests ──────────

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// Build a short unique Unix socket path (must fit in `sun_path`, ~104 bytes).
fn unique_socket_name(label: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    label.hash(&mut hasher);
    let label_hash = hasher.finish() as u32;
    let short_suffix = unique_suffix() % 1_000_000_000;
    std::env::temp_dir()
        .join(format!(
            "zcb-{label_hash:08x}-{}-{short_suffix}.sock",
            std::process::id()
        ))
        .to_string_lossy()
        .into_owned()
}

fn prepare_test_socket(socket_name: &str) {
    let path = std::path::Path::new(socket_name);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::remove_file(path);
}

fn bind_ready_test_socket(
    socket_name: &str,
    ready_tx: &mpsc::Sender<Result<(), String>>,
) -> io::Result<interprocess::local_socket::Listener> {
    prepare_test_socket(socket_name);
    let name = match local_socket_name(socket_name) {
        Ok(name) => name,
        Err(err) => {
            let _ = ready_tx.send(Err(err.to_string()));
            return Err(err);
        }
    };
    match ListenerOptions::new().name(name).create_sync() {
        Ok(listener) => {
            ready_tx.send(Ok(())).unwrap();
            Ok(listener)
        }
        Err(err) => {
            let _ = ready_tx.send(Err(err.to_string()));
            Err(err)
        }
    }
}

fn await_test_socket_ready(ready_rx: &mpsc::Receiver<Result<(), String>>, display_path: &str) {
    match ready_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("failed to bind test socket {display_path}: {err}"),
        Err(err) => panic!("timed out waiting for test socket {display_path}: {err}"),
    }
}

// ── Broker: negotiate one Hello, point client at the zccache daemon socket ──

fn zccache_service_definition(version: &str) -> ServiceDefinition {
    ServiceDefinition {
        service_name: SPIKE_SERVICE.into(),
        binary_path: "/usr/local/bin/zccache".into(),
        isolation: BrokerIsolation::SharedBroker as i32,
        explicit_instance: String::new(),
        per_version_binary_dir: String::new(),
        min_version: version.into(),
        version_allow_list: vec![version.into()],
        labels: Default::default(),
    }
}

/// Spawn a real running-process broker that negotiates one Hello and returns
/// `daemon_socket` as the negotiated backend endpoint. Copied from
/// running-process 4.4.0 `tests/broker/into_backend_io.rs::spawn_broker`.
fn spawn_broker(
    broker_socket: String,
    daemon_socket: String,
    version: String,
) -> thread::JoinHandle<io::Result<()>> {
    let display = broker_socket.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let listener = bind_ready_test_socket(&broker_socket, &ready_tx)?;
        let handler = HelloHandler::new()
            .with_backend(RegisteredBackend {
                service_definition: zccache_service_definition(&version),
                daemon_version: version.clone(),
                backend_pipe: daemon_socket.clone(),
                server_capabilities: 0x01,
            })
            .map_err(|err| io::Error::other(err.to_string()))?;
        let mut stream = listener.accept()?;
        let peer = PeerIdentity {
            pid: std::process::id(),
            uid_or_sid: "zccache-peer".into(),
        };
        handle_hello_connection(&mut stream, &handler, peer)
            .map_err(|err| io::Error::other(err.to_string()))?;
        let _ = std::fs::remove_file(&broker_socket);
        Ok(())
    });
    await_test_socket_ready(&ready_rx, &display);
    handle
}

/// Start an isolated real zccache daemon, returning its endpoint + shutdown.
fn start_daemon(
    temp: &tempfile::TempDir,
) -> (
    String,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<tokio::sync::Notify>,
) {
    let endpoint = zccache::ipc::unique_test_endpoint();
    let cache_dir: zccache::core::NormalizedPath = temp.path().into();
    let mut server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });
    (endpoint, handle, shutdown)
}

#[tokio::test]
async fn into_backend_io_socket_pings_real_zccache_daemon() {
    let version = zccache::core::VERSION.to_string();

    // 1) Real zccache daemon on its own socket.
    let temp = tempfile::tempdir().unwrap();
    let (daemon_endpoint, server_handle, shutdown) = start_daemon(&temp);

    // 2) Real running-process broker registering that socket as the backend.
    let broker_socket = unique_socket_name("zc-broker");
    let broker = spawn_broker(
        broker_socket.clone(),
        daemon_endpoint.clone(),
        version.clone(),
    );

    // 3) Adopt through the broker -> take the live negotiated socket.
    let request = OwnedConnectRequest::new(
        broker_socket.clone(),
        SPIKE_SERVICE,
        version.as_str(),
        version.as_str(),
    );
    let session = AsyncBrokerSession::adopt(request)
        .await
        .expect("broker session adopt");
    assert_eq!(
        session.route(),
        BackendConnectionRoute::BrokerNegotiated,
        "expected the broker-negotiated route (client dials the registered backend)"
    );
    assert_eq!(
        session.endpoint(),
        daemon_endpoint,
        "negotiated endpoint must be the zccache daemon socket"
    );

    // OwnedFd -> std UnixStream -> nonblocking -> tokio UnixStream ->
    // IpcConnection::from_unix_stream.
    let fd = session
        .into_backend_io()
        .expect("into_backend_io on unix")
        .into_owned_fd();
    let std_stream = std::os::unix::net::UnixStream::from(fd);
    std_stream
        .set_nonblocking(true)
        .expect("set nonblocking on adopted socket");
    let tokio_stream =
        tokio::net::UnixStream::from_std(std_stream).expect("tokio UnixStream from adopted socket");
    let mut client = zccache::ipc::IpcConnection::from_unix_stream(tokio_stream);

    // 4) Speak zccache's FrameV1 wire over the adopted socket and expect Pong.
    client
        .send_request(&Request::Ping, WireFormat::FrameV1)
        .await
        .expect("send FrameV1 Ping over adopted socket");
    let response = client
        .recv_response_with_timeout(Duration::from_secs(5))
        .await
        .expect("recv response over adopted socket");
    assert_eq!(
        response,
        Some(Response::Pong),
        "adopted broker socket must carry zccache FrameV1 Ping->Pong to the real daemon"
    );

    drop(client);
    shutdown.notify_one();
    server_handle.await.unwrap();
    broker.join().unwrap().unwrap();
}

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;

#[tokio::test]
async fn listener_connect_roundtrip_and_peer_identity() {
    let endpoint = Endpoint::unique_test("roundtrip");
    let mut listener = Listener::bind(&endpoint).expect("bind");
    let client = connect(&endpoint).await.expect("connect");
    let (mut server, peer) = listener.accept().await.expect("accept");
    assert!(peer.is_current_user());

    let (mut reader, mut writer) = tokio::io::split(client);
    let send = tokio::spawn(async move { writer.write_all(b"ping").await });
    let mut request = [0; 4];
    server.read_exact(&mut request).await.expect("read request");
    assert_eq!(&request, b"ping");
    server.write_all(b"pong").await.expect("write response");
    send.await.expect("send task").expect("send");
    let mut response = [0; 4];
    reader
        .read_exact(&mut response)
        .await
        .expect("read response");
    assert_eq!(&response, b"pong");
}

#[tokio::test]
async fn saturated_bidirectional_stream_does_not_deadlock() {
    let endpoint = Endpoint::unique_test("saturated-duplex");
    let mut listener = Listener::bind(&endpoint).expect("bind");
    let client = connect(&endpoint).await.expect("connect");
    let (server, _) = listener.accept().await.expect("accept");
    let payload_len = 1024 * 1024;

    let exchange = |stream: Stream, outgoing: u8, incoming: u8| async move {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let send = tokio::spawn(async move {
            writer
                .write_all(&vec![outgoing; payload_len])
                .await
                .expect("write saturated payload");
        });
        let mut received = vec![0; payload_len];
        reader
            .read_exact(&mut received)
            .await
            .expect("read saturated payload");
        send.await.expect("send task");
        assert!(received.iter().all(|byte| *byte == incoming));
    };

    tokio::join!(exchange(client, 0xA5, 0x5A), exchange(server, 0x5A, 0xA5));
}

#[tokio::test]
async fn listener_supports_two_sequential_accepts() {
    let endpoint = Endpoint::unique_test("sequential");
    let mut listener = Listener::bind(&endpoint).expect("bind");
    for _ in 0..2 {
        let _client = connect(&endpoint).await.expect("connect");
        let (_server, peer) = listener.accept().await.expect("accept");
        assert!(peer.is_current_user());
    }
}

#[tokio::test]
async fn missing_endpoint_has_stable_not_found_or_refused_error() {
    let endpoint = Endpoint::unique_test("missing");
    let error = connect(&endpoint)
        .await
        .err()
        .expect("missing endpoint must fail");
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    ));
}

#[test]
fn endpoint_formatting_and_running_process_conversion_round_trip() {
    let endpoint = Endpoint::select("/tmp/zccache-platform-format.sock", "zccache-format");
    assert!(!endpoint.as_str().is_empty());
    assert_eq!(endpoint.to_string(), endpoint.as_str());
    let translated = endpoint.to_running_process();
    assert_eq!(Endpoint::from_running_process(translated), endpoint);
}

#[tokio::test]
async fn endpoint_can_be_retired_and_rebound() {
    let endpoint = Endpoint::unique_test("retire-rebind");
    let listener = Listener::bind(&endpoint).expect("first bind");
    drop(listener);
    endpoint.retire().expect("retire");
    let _listener = Listener::bind(&endpoint).expect("rebind");
}

#[cfg(unix)]
#[test]
fn retirement_refuses_to_delete_an_ordinary_file() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("ordinary-file");
    std::fs::write(&path, b"preserve me").expect("write ordinary file");
    let endpoint = Endpoint::from_native(path.to_string_lossy().into_owned());

    let error = endpoint
        .retire()
        .expect_err("ordinary file must be preserved");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        std::fs::read(&path).expect("ordinary file remains"),
        b"preserve me"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn binding_tightens_parent_and_socket_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("tempdir");
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o777))
        .expect("make parent permissive");
    let path = directory.path().join("private.sock");
    let endpoint = Endpoint::from_native(path.to_string_lossy().into_owned());
    let _listener = Listener::bind(&endpoint).expect("bind secure socket");

    let parent_mode = std::fs::metadata(directory.path())
        .expect("parent metadata")
        .permissions()
        .mode()
        & 0o777;
    let socket_mode = std::fs::metadata(path)
        .expect("socket metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(parent_mode, 0o700);
    assert_eq!(socket_mode, 0o600);
}

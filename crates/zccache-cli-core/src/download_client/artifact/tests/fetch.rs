//! End-to-end `DownloadClient::fetch` / `exists` integration tests.

use std::fs::{self, File};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::download_client::DownloadClient;

use super::super::{FetchRequest, FetchStateKind, FetchStatus, WaitMode};
use super::{
    fetch_with_retry, run_with_self_healing, sha256_hex, try_wait_for_test_condition, TestDaemon,
    TestHttpConfig, TestHttpServer,
};

#[test]
fn fetch_cache_miss_then_hit_and_exists_stay_local() {
    let daemon = TestDaemon::start();
    let body = b"artifact payload".to_vec();
    let server = TestHttpServer::start(TestHttpConfig {
        body: Arc::new(body.clone()),
        accept_ranges: false,
        send_content_length: true,
        chunk_size: 0,
        chunk_delay: Duration::ZERO,
        path: "artifact.bin".to_string(),
        request_started: None,
        release_response: None,
    });
    let client = DownloadClient::new(Some(daemon.endpoint.clone()));
    let dir = tempfile::tempdir().unwrap();
    let mut request = FetchRequest::new(server.url.clone(), dir.path().join("artifact.bin"));
    request.expected_sha256 = Some(sha256_hex(&body));

    let first = fetch_with_retry(&client, request.clone()).unwrap();
    assert_eq!(first.status, FetchStatus::Downloaded);
    assert_eq!(first.sha256, sha256_hex(&body));
    let requests_after_first = server.request_count();
    assert!(requests_after_first > 0);

    let second = fetch_with_retry(&client, request.clone()).unwrap();
    assert_eq!(second.status, FetchStatus::AlreadyPresent);
    assert_eq!(server.request_count(), requests_after_first);

    let state = client.exists(&request).unwrap();
    assert_eq!(state.kind, FetchStateKind::ArtifactReady);
    assert_eq!(state.sha256.as_deref(), Some(first.sha256.as_str()));
    assert_eq!(server.request_count(), requests_after_first);
}

#[test]
fn fetch_checksum_mismatch_cleans_up_invalid_artifact() {
    let daemon = TestDaemon::start();
    let body = b"wrong checksum body".to_vec();
    let server = TestHttpServer::start(TestHttpConfig {
        body: Arc::new(body),
        accept_ranges: false,
        send_content_length: true,
        chunk_size: 0,
        chunk_delay: Duration::ZERO,
        path: "bad.bin".to_string(),
        request_started: None,
        release_response: None,
    });
    let client = DownloadClient::new(Some(daemon.endpoint.clone()));
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("bad.bin");
    let mut request = FetchRequest::new(server.url.clone(), &destination);
    request.expected_sha256 = Some("00".repeat(32));

    let err = fetch_with_retry(&client, request.clone()).unwrap_err();
    assert!(err.contains("sha256 mismatch"));
    assert!(!destination.exists());

    let state = client.exists(&request).unwrap();
    assert_eq!(state.kind, FetchStateKind::Missing);
}

#[test]
fn fetch_single_url_max_connections_uses_range_requests() {
    let daemon = TestDaemon::start();
    let body: Vec<u8> = (0..128 * 1024).map(|i| (i % 251) as u8).collect();
    let server = TestHttpServer::start(TestHttpConfig {
        body: Arc::new(body.clone()),
        accept_ranges: true,
        send_content_length: true,
        chunk_size: 4096,
        chunk_delay: Duration::ZERO,
        path: "multipart.bin".to_string(),
        request_started: None,
        release_response: None,
    });
    let client = DownloadClient::new(Some(daemon.endpoint.clone()));
    let dir = tempfile::tempdir().unwrap();
    let mut request = FetchRequest::new(server.url.clone(), dir.path().join("multipart.bin"));
    request.download_options.max_connections = Some(4);
    request.download_options.min_segment_size = Some(1024);
    request.expected_sha256 = Some(sha256_hex(&body));

    let result = fetch_with_retry(&client, request).unwrap();
    assert_eq!(result.status, FetchStatus::Downloaded);
    assert_eq!(result.sha256, sha256_hex(&body));
    assert!(server.range_request_count() >= 2);
}

#[test]
fn fetch_explicit_multipart_urls_concatenates_and_stays_local() {
    let daemon = TestDaemon::start();
    let part_a = b"hello ".to_vec();
    let part_b = b"multipart ".to_vec();
    let part_c = b"world".to_vec();
    let mut full = Vec::new();
    full.extend_from_slice(&part_a);
    full.extend_from_slice(&part_b);
    full.extend_from_slice(&part_c);

    let server_a = TestHttpServer::start(TestHttpConfig {
        body: Arc::new(part_a),
        accept_ranges: false,
        send_content_length: true,
        chunk_size: 0,
        chunk_delay: Duration::ZERO,
        path: "artifact.part-aa".to_string(),
        request_started: None,
        release_response: None,
    });
    let server_b = TestHttpServer::start(TestHttpConfig {
        body: Arc::new(part_b),
        accept_ranges: false,
        send_content_length: true,
        chunk_size: 0,
        chunk_delay: Duration::ZERO,
        path: "artifact.part-ab".to_string(),
        request_started: None,
        release_response: None,
    });
    let server_c = TestHttpServer::start(TestHttpConfig {
        body: Arc::new(part_c),
        accept_ranges: false,
        send_content_length: true,
        chunk_size: 0,
        chunk_delay: Duration::ZERO,
        path: "artifact.part-ac".to_string(),
        request_started: None,
        release_response: None,
    });

    let client = DownloadClient::new(Some(daemon.endpoint.clone()));
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("artifact.bin");
    let mut request = FetchRequest::new(
        vec![
            server_a.url.clone(),
            server_b.url.clone(),
            server_c.url.clone(),
        ],
        &destination,
    );
    request.expected_sha256 = Some(sha256_hex(&full));

    let first = fetch_with_retry(&client, request.clone()).unwrap();
    assert_eq!(first.status, FetchStatus::Downloaded);
    assert_eq!(first.sha256, sha256_hex(&full));
    assert_eq!(fs::read(&destination).unwrap(), full);
    let request_counts = (
        server_a.request_count(),
        server_b.request_count(),
        server_c.request_count(),
    );

    let second = fetch_with_retry(&client, request.clone()).unwrap();
    assert_eq!(second.status, FetchStatus::AlreadyPresent);
    assert_eq!(
        (
            server_a.request_count(),
            server_b.request_count(),
            server_c.request_count()
        ),
        request_counts
    );

    let state = client.exists(&request).unwrap();
    assert_eq!(state.kind, FetchStateKind::ArtifactReady);
    assert_eq!(state.sha256.as_deref(), Some(first.sha256.as_str()));
}

#[test]
fn fetch_no_wait_returns_locked_while_other_client_is_downloading() {
    run_with_self_healing(
        "fetch_no_wait_returns_locked_while_other_client_is_downloading",
        |attempt| {
            let daemon = TestDaemon::start();
            let request_started = Arc::new(AtomicBool::new(false));
            let release_response = Arc::new(AtomicBool::new(false));
            let body: Vec<u8> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();
            let server = TestHttpServer::start(TestHttpConfig {
                body: Arc::new(body),
                accept_ranges: false,
                send_content_length: true,
                chunk_size: 4096,
                chunk_delay: Duration::from_millis(2),
                path: "slow.bin".to_string(),
                request_started: Some(Arc::clone(&request_started)),
                release_response: Some(Arc::clone(&release_response)),
            });
            let dest_dir = tempfile::tempdir().map_err(|e| e.to_string())?;
            let destination = dest_dir.path().join(format!("slow-{attempt}.bin"));

            let endpoint = daemon.endpoint.clone();
            let url = server.url.clone();
            let destination_for_thread = destination.clone();
            let download_thread = thread::spawn(move || {
                let client = DownloadClient::new(Some(endpoint));
                let request = FetchRequest::new(url, &destination_for_thread);
                fetch_with_retry(&client, request)
            });

            let outcome = (|| -> Result<(), String> {
                try_wait_for_test_condition(
                    Duration::from_secs(30),
                    "initial download request",
                    || request_started.load(Ordering::Acquire),
                )?;
                let client = DownloadClient::new(Some(daemon.endpoint.clone()));
                let mut no_wait = FetchRequest::new(server.url.clone(), &destination);
                no_wait.wait_mode = WaitMode::NoWait;
                let locked = fetch_with_retry(&client, no_wait)
                    .map_err(|err| format!("no-wait fetch failed: {err}"))?;
                if locked.status != FetchStatus::Locked {
                    return Err(format!("expected Locked status, got {:?}", locked.status));
                }
                Ok(())
            })();

            release_response.store(true, Ordering::Release);
            let join_result = download_thread.join();
            outcome?;
            let completed = match join_result {
                Ok(Ok(result)) => result,
                Ok(Err(err)) => return Err(format!("download thread returned error: {err}")),
                Err(_) => return Err("download thread panicked".to_string()),
            };
            if completed.status != FetchStatus::Downloaded {
                return Err(format!(
                    "expected Downloaded status, got {:?}",
                    completed.status
                ));
            }
            Ok(())
        },
    );
}

#[test]
fn fetch_multipart_no_wait_returns_locked_while_other_client_is_downloading() {
    run_with_self_healing(
        "fetch_multipart_no_wait_returns_locked_while_other_client_is_downloading",
        |attempt| {
            let daemon = TestDaemon::start();
            let request_started = Arc::new(AtomicBool::new(false));
            let release_response = Arc::new(AtomicBool::new(false));
            let slow_server = TestHttpServer::start(TestHttpConfig {
                body: Arc::new((0..512 * 1024).map(|i| (i % 251) as u8).collect()),
                accept_ranges: false,
                send_content_length: true,
                chunk_size: 4096,
                chunk_delay: Duration::from_millis(2),
                path: "slow.part-aa".to_string(),
                request_started: Some(Arc::clone(&request_started)),
                release_response: Some(Arc::clone(&release_response)),
            });
            let fast_server = TestHttpServer::start(TestHttpConfig {
                body: Arc::new(b"tail".to_vec()),
                accept_ranges: false,
                send_content_length: true,
                chunk_size: 0,
                chunk_delay: Duration::ZERO,
                path: "slow.part-ab".to_string(),
                request_started: None,
                release_response: None,
            });
            let dest_dir = tempfile::tempdir().map_err(|e| e.to_string())?;
            let destination = dest_dir.path().join(format!("slow-{attempt}.bin"));

            let endpoint = daemon.endpoint.clone();
            let source = vec![slow_server.url.clone(), fast_server.url.clone()];
            let destination_for_thread = destination.clone();
            let download_thread = thread::spawn(move || {
                let client = DownloadClient::new(Some(endpoint));
                let request = FetchRequest::new(source, &destination_for_thread);
                fetch_with_retry(&client, request)
            });

            let outcome = (|| -> Result<(), String> {
                try_wait_for_test_condition(
                    Duration::from_secs(30),
                    "initial multipart download request",
                    || request_started.load(Ordering::Acquire),
                )?;
                let client = DownloadClient::new(Some(daemon.endpoint.clone()));
                let mut no_wait = FetchRequest::new(
                    vec![slow_server.url.clone(), fast_server.url.clone()],
                    &destination,
                );
                no_wait.wait_mode = WaitMode::NoWait;
                let locked = fetch_with_retry(&client, no_wait)
                    .map_err(|err| format!("no-wait fetch failed: {err}"))?;
                if locked.status != FetchStatus::Locked {
                    return Err(format!("expected Locked status, got {:?}", locked.status));
                }
                Ok(())
            })();

            release_response.store(true, Ordering::Release);
            let join_result = download_thread.join();
            outcome?;
            let completed = match join_result {
                Ok(Ok(result)) => result,
                Ok(Err(err)) => return Err(format!("download thread returned error: {err}")),
                Err(_) => return Err("download thread panicked".to_string()),
            };
            if completed.status != FetchStatus::Downloaded {
                return Err(format!(
                    "expected Downloaded status, got {:?}",
                    completed.status
                ));
            }
            Ok(())
        },
    );
}

#[test]
fn fetch_dry_run_avoids_network_and_filesystem_mutation() {
    let daemon = TestDaemon::start();
    let server = TestHttpServer::start(TestHttpConfig {
        body: Arc::new(b"dry-run".to_vec()),
        accept_ranges: false,
        send_content_length: true,
        chunk_size: 0,
        chunk_delay: Duration::ZERO,
        path: "dry.bin".to_string(),
        request_started: None,
        release_response: None,
    });
    let client = DownloadClient::new(Some(daemon.endpoint.clone()));
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("dry.bin");
    let mut request = FetchRequest::new(server.url.clone(), &destination);
    request.dry_run = true;

    let result = fetch_with_retry(&client, request).unwrap();
    assert_eq!(result.status, FetchStatus::DryRun);
    assert_eq!(server.request_count(), 0);
    assert!(!destination.exists());
}

#[test]
fn fetch_expands_7z_and_exists_reports_expanded_ready() {
    let daemon = TestDaemon::start();
    let dir = tempfile::tempdir().unwrap();
    let source_dir = dir.path().join("source");
    fs::create_dir_all(source_dir.join("bin")).unwrap();
    fs::write(source_dir.join("bin").join("tool.txt"), b"tool data").unwrap();
    let archive_path = dir.path().join("toolchain.7z");
    sevenz_rust::compress_to_path(&source_dir, &archive_path).unwrap();
    let archive_bytes = fs::read(&archive_path).unwrap();

    let server = TestHttpServer::start(TestHttpConfig {
        body: Arc::new(archive_bytes.clone()),
        accept_ranges: false,
        send_content_length: true,
        chunk_size: 0,
        chunk_delay: Duration::ZERO,
        path: "toolchain.7z".to_string(),
        request_started: None,
        release_response: None,
    });
    let client = DownloadClient::new(Some(daemon.endpoint.clone()));
    let cache_path = dir.path().join("cache").join("toolchain.7z");
    let expanded_path = dir.path().join("expanded");
    let mut request = FetchRequest::new(server.url.clone(), &cache_path);
    request.destination_path_expanded = Some(expanded_path.clone().into());
    request.expected_sha256 = Some(sha256_hex(&archive_bytes));

    let first = fetch_with_retry(&client, request.clone()).unwrap();
    assert_eq!(first.status, FetchStatus::Expanded);
    assert_eq!(first.sha256, sha256_hex(&archive_bytes));
    let extracted = [
        expanded_path.join("source").join("bin").join("tool.txt"),
        expanded_path.join("bin").join("tool.txt"),
        expanded_path.join("tool.txt"),
    ]
    .into_iter()
    .find(|path| path.exists())
    .expect("expected extracted file in expanded directory");
    assert_eq!(fs::read(extracted).unwrap(), b"tool data");

    let state = client.exists(&request).unwrap();
    assert_eq!(state.kind, FetchStateKind::ExpandedReady);
    assert_eq!(state.sha256.as_deref(), Some(first.sha256.as_str()));

    let second = fetch_with_retry(&client, request).unwrap();
    assert_eq!(second.status, FetchStatus::AlreadyExpanded);
    assert_eq!(second.sha256, first.sha256);
}

#[test]
fn fetch_without_expected_sha_then_validate_later_uses_stored_fingerprint() {
    let daemon = TestDaemon::start();
    let body = b"artifact with delayed hash".to_vec();
    let server = TestHttpServer::start(TestHttpConfig {
        body: Arc::new(body.clone()),
        accept_ranges: false,
        send_content_length: true,
        chunk_size: 0,
        chunk_delay: Duration::ZERO,
        path: "delayed.bin".to_string(),
        request_started: None,
        release_response: None,
    });
    let client = DownloadClient::new(Some(daemon.endpoint.clone()));
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("delayed.bin");

    let first =
        fetch_with_retry(&client, FetchRequest::new(server.url.clone(), &destination)).unwrap();
    assert_eq!(first.status, FetchStatus::Downloaded);
    assert_eq!(first.sha256, sha256_hex(&body));

    let mut later = FetchRequest::new(server.url.clone(), &destination);
    later.expected_sha256 = Some(first.sha256.clone());
    let second = fetch_with_retry(&client, later.clone()).unwrap();
    assert_eq!(second.status, FetchStatus::AlreadyPresent);
    assert_eq!(second.sha256, first.sha256);

    let state = client.exists(&later).unwrap();
    assert_eq!(state.kind, FetchStateKind::ArtifactReady);
    assert_eq!(state.sha256.as_deref(), Some(second.sha256.as_str()));
}

#[test]
fn expanded_state_remains_valid_when_expected_sha_is_added_later() {
    let daemon = TestDaemon::start();
    let dir = tempfile::tempdir().unwrap();
    let archive_path = dir.path().join("bundle.zip");
    {
        let file = File::create(&archive_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("hello.txt", options).unwrap();
        zip.write_all(b"hello").unwrap();
        zip.finish().unwrap();
    }
    let archive_bytes = fs::read(&archive_path).unwrap();
    let server = TestHttpServer::start(TestHttpConfig {
        body: Arc::new(archive_bytes.clone()),
        accept_ranges: false,
        send_content_length: true,
        chunk_size: 0,
        chunk_delay: Duration::ZERO,
        path: "bundle.zip".to_string(),
        request_started: None,
        release_response: None,
    });
    let client = DownloadClient::new(Some(daemon.endpoint.clone()));
    let cache_path = dir.path().join("cache").join("bundle.zip");
    let expanded_path = dir.path().join("expanded");

    let mut initial = FetchRequest::new(server.url.clone(), &cache_path);
    initial.destination_path_expanded = Some(expanded_path.clone().into());
    let first = fetch_with_retry(&client, initial).unwrap();
    assert_eq!(first.status, FetchStatus::Expanded);

    let mut later = FetchRequest::new(server.url.clone(), &cache_path);
    later.destination_path_expanded = Some(expanded_path.clone().into());
    later.expected_sha256 = Some(first.sha256.clone());
    let second = fetch_with_retry(&client, later.clone()).unwrap();
    assert_eq!(second.status, FetchStatus::AlreadyExpanded);
    assert_eq!(second.sha256, first.sha256);

    let state = client.exists(&later).unwrap();
    assert_eq!(state.kind, FetchStateKind::ExpandedReady);
    assert_eq!(state.sha256.as_deref(), Some(second.sha256.as_str()));
}

#[test]
fn force_is_rejected_for_existing_artifact_state() {
    let daemon = TestDaemon::start();
    let body = b"immutable".to_vec();
    let server = TestHttpServer::start(TestHttpConfig {
        body: Arc::new(body),
        accept_ranges: false,
        send_content_length: true,
        chunk_size: 0,
        chunk_delay: Duration::ZERO,
        path: "immutable.bin".to_string(),
        request_started: None,
        release_response: None,
    });
    let client = DownloadClient::new(Some(daemon.endpoint.clone()));
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("immutable.bin");

    let _ = fetch_with_retry(&client, FetchRequest::new(server.url.clone(), &destination)).unwrap();

    let mut force = FetchRequest::new(server.url.clone(), &destination);
    force.force = true;
    let err = fetch_with_retry(&client, force).unwrap_err();
    assert!(err.contains("purge"));
}

#[test]
fn fetch_rejects_unsafe_zip_entries_end_to_end() {
    let daemon = TestDaemon::start();
    let dir = tempfile::tempdir().unwrap();
    let archive_path = dir.path().join("unsafe.zip");
    {
        let file = File::create(&archive_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("../evil.txt", options).unwrap();
        zip.write_all(b"bad").unwrap();
        zip.finish().unwrap();
    }
    let archive_bytes = fs::read(&archive_path).unwrap();
    let server = TestHttpServer::start(TestHttpConfig {
        body: Arc::new(archive_bytes),
        accept_ranges: false,
        send_content_length: true,
        chunk_size: 0,
        chunk_delay: Duration::ZERO,
        path: "unsafe.zip".to_string(),
        request_started: None,
        release_response: None,
    });
    let client = DownloadClient::new(Some(daemon.endpoint.clone()));
    let cache_path = dir.path().join("cache").join("unsafe.zip");
    let expanded_path = dir.path().join("expanded");
    let mut request = FetchRequest::new(server.url.clone(), &cache_path);
    request.destination_path_expanded = Some(expanded_path.clone().into());

    let err = fetch_with_retry(&client, request).unwrap_err();
    assert!(err.contains("unsafe zip entry"));
    assert!(!dir.path().join("evil.txt").exists());
}

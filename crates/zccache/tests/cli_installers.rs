#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
};
use std::thread::{self, JoinHandle};

use tempfile::TempDir;
use zccache::core::NormalizedPath;

const VERSION: &str = "1.2.3";

fn installer_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn workspace_root() -> NormalizedPath {
    NormalizedPath::new(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crate dir")
            .parent()
            .expect("workspace root"),
    )
}

struct TestServer {
    addr: String,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TestServer {
    fn start(root: NormalizedPath) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        listener
            .set_nonblocking(true)
            .expect("set test server nonblocking");
        let addr = format!("http://{}", listener.local_addr().expect("server addr"));
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => serve_connection(stream, &root),
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(addr) = self
            .addr
            .strip_prefix("http://")
            .unwrap_or(&self.addr)
            .parse::<std::net::SocketAddr>()
        {
            let _ = TcpStream::connect(addr).and_then(|stream| stream.shutdown(Shutdown::Both));
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve_connection(mut stream: TcpStream, root: &NormalizedPath) {
    let mut buf = [0_u8; 4096];
    let Ok(size) = stream.read(&mut buf) else {
        return;
    };
    if size == 0 {
        return;
    }
    let request = String::from_utf8_lossy(&buf[..size]);
    let Some(line) = request.lines().next() else {
        return;
    };
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or("/");
    if path == "/latest" {
        let _ = write_redirect(&mut stream, &format!("/tag/{VERSION}"));
        return;
    }
    if path == format!("/tag/{VERSION}") {
        let _ = write_response(&mut stream, 200, b"release page");
        return;
    }
    if method != "GET" {
        let _ = write_response(&mut stream, 405, b"method not allowed");
        return;
    }

    let rel = path
        .trim_start_matches('/')
        .replace('/', std::path::MAIN_SEPARATOR_STR);
    let file_path = root.join(rel);
    match fs::read(file_path) {
        Ok(bytes) => {
            let _ = write_response(&mut stream, 200, &bytes);
        }
        Err(_) => {
            let _ = write_response(&mut stream, 404, b"not found");
        }
    }
}

fn write_redirect(stream: &mut TcpStream, location: &str) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

fn write_response(stream: &mut TcpStream, status: u16, body: &[u8]) -> std::io::Result<()> {
    let status_text = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {status_text}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)
}

#[cfg(unix)]
fn make_fake_unix_archive(root: &Path, target: &str) -> NormalizedPath {
    let release_tag = VERSION;
    let archive_tag = format!("v{VERSION}");
    let asset_dir = root.join("download").join(release_tag);
    let archive_root = asset_dir.join(format!("zccache-{archive_tag}-{target}"));
    fs::create_dir_all(&archive_root).expect("create archive root");
    write_unix_binary(&archive_root.join("zccache"), "zccache");
    write_unix_binary(&archive_root.join("zccache-daemon"), "zccache-daemon");
    write_unix_binary(&archive_root.join("zccache-fp"), "zccache-fp");
    fs::write(archive_root.join("README.md"), "test archive\n").expect("write readme");

    let archive = asset_dir.join(format!("zccache-{archive_tag}-{target}.tar.gz"));
    let status = Command::new("tar")
        .args([
            "-czf",
            archive.to_str().expect("archive path"),
            "-C",
            asset_dir.to_str().expect("asset dir"),
            &format!("zccache-{archive_tag}-{target}"),
        ])
        .status()
        .expect("run tar");
    assert!(status.success(), "tar failed with {status}");
    NormalizedPath::new(archive)
}

#[cfg(unix)]
fn write_unix_binary(path: &Path, name: &str) {
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  echo '{name} {VERSION}'\n  exit 0\nfi\necho '{name} invoked'\n"
    );
    fs::write(path, script).expect("write fake unix binary");
    let mut perms = fs::metadata(path).expect("unix metadata").permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod fake unix binary");
}

#[cfg(windows)]
fn make_fake_windows_archive(root: &Path, target: &str) -> NormalizedPath {
    let release_tag = VERSION;
    let archive_tag = format!("v{VERSION}");
    let asset_dir = root.join("download").join(release_tag);
    let archive_root = asset_dir.join(format!("zccache-{archive_tag}-{target}"));
    fs::create_dir_all(&archive_root).expect("create archive root");
    write_windows_binary(&archive_root.join("zccache.exe"), "zccache");
    write_windows_binary(&archive_root.join("zccache-daemon.exe"), "zccache-daemon");
    write_windows_binary(&archive_root.join("zccache-fp.exe"), "zccache-fp");
    fs::write(archive_root.join("README.md"), "test archive\r\n").expect("write readme");

    let archive = asset_dir.join(format!("zccache-{archive_tag}-{target}.zip"));
    write_zip_archive(&archive, &asset_dir, &archive_root);
    NormalizedPath::new(archive)
}

#[cfg(windows)]
fn write_windows_binary(path: &Path, name: &str) {
    let source = format!(
        "using System; class Program {{ static int Main(string[] args) {{ if (args.Length > 0 && args[0] == \"--version\") {{ Console.WriteLine(\"{name} {VERSION}\"); return 0; }} Console.WriteLine(\"{name} invoked\"); return 0; }} }}"
    );
    let command = format!(
        "$src = @'\n{source}\n'@; Add-Type -TypeDefinition $src -OutputAssembly '{}' -OutputType ConsoleApplication",
        path.display()
    );
    let status = Command::new("powershell")
        .args(["-NoProfile", "-Command", &command])
        .status()
        .expect("run Add-Type");
    assert!(status.success(), "Add-Type failed with {status}");
}

#[cfg(windows)]
fn write_zip_archive(archive: &Path, base_dir: &Path, root_dir: &Path) {
    use std::fs::File;

    use zip::write::SimpleFileOptions;
    use zip::CompressionMethod;
    use zip::ZipWriter;

    if archive.exists() {
        fs::remove_file(archive).expect("remove existing archive");
    }

    let file = File::create(archive).expect("create zip archive");
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    add_directory_to_zip(&mut zip, base_dir, root_dir, options);
    zip.finish().expect("finish zip archive");
}

#[cfg(windows)]
fn add_directory_to_zip(
    zip: &mut zip::ZipWriter<std::fs::File>,
    base_dir: &Path,
    dir: &Path,
    options: zip::write::SimpleFileOptions,
) {
    let dir_name = dir
        .strip_prefix(base_dir)
        .expect("directory inside base")
        .to_string_lossy()
        .replace('\\', "/");
    zip.add_directory(format!("{dir_name}/"), options)
        .expect("add zip directory");

    let mut entries = fs::read_dir(dir)
        .expect("read zip source dir")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect zip source dir");
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            add_directory_to_zip(zip, base_dir, &path, options);
            continue;
        }

        let name = path
            .strip_prefix(base_dir)
            .expect("file inside base")
            .to_string_lossy()
            .replace('\\', "/");
        zip.start_file(name, options).expect("start zip file");

        let bytes = fs::read(&path).expect("read zip file bytes");
        zip.write_all(&bytes).expect("write zip file bytes");
    }
}

#[cfg(unix)]
#[test]
fn install_sh_installs_release_archive() {
    let _guard = installer_test_lock().lock().expect("installer test lock");
    let temp = TempDir::new().expect("tempdir");
    let release_root = temp.path().join("release");
    fs::create_dir_all(&release_root).expect("create release root");
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        other => panic!("unsupported test target: {other:?}"),
    };
    let _archive = make_fake_unix_archive(&release_root, target);
    let server = TestServer::start(release_root.into());

    let home = temp.path().join("home");
    let install_dir = temp.path().join("bin");
    fs::create_dir_all(&home).expect("create home");

    let status = Command::new("sh")
        .arg(workspace_root().join("install.sh"))
        .args(["--bin-dir", install_dir.to_str().expect("install dir")])
        .env("HOME", &home)
        .env("SHELL", "/bin/bash")
        .env(
            "ZCCACHE_INSTALL_BASE_URL",
            format!("{}/download-placeholder", server.addr).replace("/download-placeholder", ""),
        )
        .env("ZCCACHE_INSTALL_VERSION", VERSION)
        .status()
        .expect("run install.sh");
    assert!(status.success(), "install.sh failed with {status}");

    let version = Command::new(install_dir.join("zccache"))
        .arg("--version")
        .output()
        .expect("run installed zccache");
    assert!(version.status.success(), "installed zccache failed");
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        format!("zccache {VERSION}")
    );

    let profile = fs::read_to_string(home.join(".profile")).expect("read profile");
    assert!(
        profile.contains(install_dir.to_str().expect("install dir")),
        "profile missing install path"
    );
    assert!(
        install_dir.join("zccache-daemon").exists(),
        "daemon not installed"
    );
    assert!(
        install_dir.join("zccache-fp").exists(),
        "fingerprint tool not installed"
    );
}

#[cfg(unix)]
#[test]
fn install_sh_supports_global_mode() {
    let _guard = installer_test_lock().lock().expect("installer test lock");
    let temp = TempDir::new().expect("tempdir");
    let release_root = temp.path().join("release");
    fs::create_dir_all(&release_root).expect("create release root");
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        other => panic!("unsupported test target: {other:?}"),
    };
    let _archive = make_fake_unix_archive(&release_root, target);
    let server = TestServer::start(release_root.into());

    let home = temp.path().join("home");
    let install_dir = temp.path().join("global-bin");
    fs::create_dir_all(&home).expect("create home");

    let status = Command::new("sh")
        .arg(workspace_root().join("install.sh"))
        .args([
            "--global",
            "--bin-dir",
            install_dir.to_str().expect("install dir"),
        ])
        .env("HOME", &home)
        .env("SHELL", "/bin/bash")
        .env("ZCCACHE_INSTALL_BASE_URL", &server.addr)
        .env("ZCCACHE_INSTALL_VERSION", VERSION)
        .status()
        .expect("run install.sh");
    assert!(status.success(), "install.sh failed with {status}");
    assert!(
        install_dir.join("zccache").exists(),
        "zccache not installed"
    );
    assert!(
        !home.join(".profile").exists(),
        "global install should not edit user profile"
    );
}

#[cfg(unix)]
#[test]
fn install_sh_resolves_latest_release_tag() {
    let _guard = installer_test_lock().lock().expect("installer test lock");
    let temp = TempDir::new().expect("tempdir");
    let release_root = temp.path().join("release");
    fs::create_dir_all(&release_root).expect("create release root");
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        other => panic!("unsupported test target: {other:?}"),
    };
    let _archive = make_fake_unix_archive(&release_root, target);
    let server = TestServer::start(release_root.into());

    let home = temp.path().join("home");
    let install_dir = temp.path().join("bin");
    fs::create_dir_all(&home).expect("create home");

    let status = Command::new("sh")
        .arg(workspace_root().join("install.sh"))
        .args(["--bin-dir", install_dir.to_str().expect("install dir")])
        .env("HOME", &home)
        .env("SHELL", "/bin/bash")
        .env("ZCCACHE_INSTALL_BASE_URL", &server.addr)
        .env("ZCCACHE_INSTALL_VERSION", "latest")
        .status()
        .expect("run install.sh");
    assert!(status.success(), "install.sh failed with {status}");

    let version = Command::new(install_dir.join("zccache"))
        .arg("--version")
        .output()
        .expect("run installed zccache");
    assert!(version.status.success(), "installed zccache failed");
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        format!("zccache {VERSION}")
    );
}

#[cfg(windows)]
#[test]
fn install_ps1_installs_release_archive() {
    let _guard = installer_test_lock().lock().expect("installer test lock");
    let temp = TempDir::new().expect("tempdir");
    let release_root = temp.path().join("release");
    fs::create_dir_all(&release_root).expect("create release root");
    let target = match std::env::consts::ARCH {
        "x86_64" => "x86_64-pc-windows-msvc",
        "aarch64" => "aarch64-pc-windows-msvc",
        other => panic!("unsupported test arch: {other}"),
    };
    let _archive = make_fake_windows_archive(&release_root, target);
    let server = TestServer::start(release_root.into());

    let home = temp.path().join("home");
    let install_dir = temp.path().join("bin");
    fs::create_dir_all(&home).expect("create home");

    let status = Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            workspace_root()
                .join("install.ps1")
                .to_str()
                .expect("install.ps1 path"),
            "-BinDir",
            install_dir.to_str().expect("install dir"),
            "-Version",
            VERSION,
        ])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env(
            "ZCCACHE_INSTALL_BASE_URL",
            format!("{}/download-placeholder", server.addr).replace("/download-placeholder", ""),
        )
        .env("ZCCACHE_NO_MODIFY_PATH", "1")
        .status()
        .expect("run install.ps1");
    assert!(status.success(), "install.ps1 failed with {status}");

    let version = Command::new(install_dir.join("zccache.exe"))
        .arg("--version")
        .output()
        .expect("run installed zccache.exe");
    assert!(version.status.success(), "installed zccache.exe failed");
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        format!("zccache {VERSION}")
    );
    assert!(
        install_dir.join("zccache-daemon.exe").exists(),
        "daemon not installed"
    );
    assert!(
        install_dir.join("zccache-fp.exe").exists(),
        "fingerprint tool not installed"
    );
}

#[cfg(windows)]
#[test]
fn install_ps1_supports_global_mode() {
    let _guard = installer_test_lock().lock().expect("installer test lock");
    let temp = TempDir::new().expect("tempdir");
    let release_root = temp.path().join("release");
    fs::create_dir_all(&release_root).expect("create release root");
    let target = match std::env::consts::ARCH {
        "x86_64" => "x86_64-pc-windows-msvc",
        "aarch64" => "aarch64-pc-windows-msvc",
        other => panic!("unsupported test arch: {other}"),
    };
    let _archive = make_fake_windows_archive(&release_root, target);
    let server = TestServer::start(release_root.into());

    let install_dir = temp.path().join("global-bin");

    let status = Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            workspace_root()
                .join("install.ps1")
                .to_str()
                .expect("install.ps1 path"),
            "-Global",
            "-BinDir",
            install_dir.to_str().expect("install dir"),
            "-Version",
            VERSION,
        ])
        .env("ZCCACHE_INSTALL_BASE_URL", &server.addr)
        .env("ZCCACHE_NO_MODIFY_PATH", "1")
        .status()
        .expect("run install.ps1");
    assert!(status.success(), "install.ps1 failed with {status}");
    assert!(
        install_dir.join("zccache.exe").exists(),
        "zccache.exe not installed"
    );
}

use std::io;
use std::os::unix::fs::FileTypeExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEST_ENDPOINT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Endpoint(String);
impl Endpoint {
    pub fn from_native(value: String) -> Self { Self(value) }
    pub fn as_str(&self) -> &str { &self.0 }
    pub fn retire(&self) -> io::Result<()> {
        match std::fs::symlink_metadata(&self.0) {
            Ok(metadata) if metadata.file_type().is_socket() => std::fs::remove_file(&self.0),
            Ok(_) => Err(io::Error::new(io::ErrorKind::InvalidInput, "IPC endpoint is not a socket")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
    pub fn unique_test(name: &str) -> Self {
        let id = TEST_ENDPOINT.fetch_add(1, Ordering::Relaxed);
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        Self(format!("/tmp/zccache-platform-{}-{nonce}-{id}/{name}.sock", std::process::id()))
    }
    pub fn select(file_path: String, _pipe_name: String) -> Self { Self(file_path) }
    pub fn to_running_process(&self) -> String { self.0.clone() }
    pub fn from_running_process(value: String) -> Self { Self(value) }
    pub fn file_path_is_portable(value: &str) -> bool { value.len() <= 100 }
    pub fn uses_file_path(&self) -> bool { true }
    pub fn connect_timeout(&self) -> std::time::Duration { std::time::Duration::from_secs(30) }
}

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEST_ENDPOINT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Endpoint(String);
impl Endpoint {
    pub fn from_native(value: String) -> Self { Self(value) }
    pub fn as_str(&self) -> &str { &self.0 }
    pub fn retire(&self) -> io::Result<()> { Ok(()) }
    pub fn unique_test(name: &str) -> Self {
        let id = TEST_ENDPOINT.fetch_add(1, Ordering::Relaxed);
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        Self(format!(r"\\.\pipe\zccache-platform-{name}-{}-{nonce}-{id}", std::process::id()))
    }
    pub fn select(_file_path: String, pipe_name: String) -> Self { Self::from_running_process(pipe_name) }
    pub fn to_running_process(&self) -> String { self.0.strip_prefix(r"\\.\pipe\").unwrap_or(&self.0).to_owned() }
    pub fn from_running_process(value: String) -> Self {
        if value.starts_with(r"\\.\pipe\") { Self(value) } else { Self(format!(r"\\.\pipe\{value}")) }
    }
    pub fn file_path_is_portable(_value: &str) -> bool { true }
    pub fn uses_file_path(&self) -> bool { false }
    pub fn connect_timeout(&self) -> std::time::Duration { std::time::Duration::from_secs(5) }
}

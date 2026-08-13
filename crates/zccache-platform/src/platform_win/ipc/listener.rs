use std::collections::VecDeque;
use std::io;
use std::time::Duration;
use tokio::net::windows::named_pipe::NamedPipeServer;
use super::{Endpoint, PeerIdentity, Stream};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REPLACEMENT_ATTEMPTS: usize = 5;
const FIRST_BIND_ATTEMPTS: usize = 8;

pub struct Listener { endpoint: String, pool: VecDeque<NamedPipeServer> }
impl Listener {
    pub fn bind(endpoint: &Endpoint) -> io::Result<Self> {
        let pool_size = std::env::var("ZCCACHE_PIPE_POOL_SIZE").ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or_else(|| std::thread::available_parallelism()
                .map(|parallelism| parallelism.get().saturating_mul(4))
                .unwrap_or(64).clamp(16, 128));
        let mut pool = VecDeque::with_capacity(pool_size);
        pool.push_back(create_first_with_retry(endpoint.as_str())?);
        for _ in 1..pool_size { pool.push_back(create_server(endpoint.as_str(), false)?); }
        Ok(Self { endpoint: endpoint.as_str().to_owned(), pool })
    }
    pub async fn accept(&mut self) -> io::Result<(Stream, PeerIdentity)> {
        loop {
            let pipe = match self.pool.pop_front() { Some(pipe) => pipe, None => create_with_retry(&self.endpoint).await? };
            match tokio::time::timeout(CONNECT_TIMEOUT, pipe.connect()).await {
                Ok(Ok(())) => {
                    if let Ok(replacement) = create_with_retry(&self.endpoint).await { self.pool.push_back(replacement); }
                    return Ok((Stream::Server(pipe), PeerIdentity { pid: None }));
                }
                Ok(Err(_)) | Err(_) => if let Ok(replacement) = create_with_retry(&self.endpoint).await { self.pool.push_back(replacement); },
            }
        }
    }
    pub fn drain_accept_pool(&mut self) -> usize { let count = self.pool.len(); self.pool.clear(); count }
    pub fn tightened_parent(&self) -> bool { false }
}
fn create_server(endpoint: &str, first: bool) -> io::Result<NamedPipeServer> { super::pipe_security::create(endpoint, first) }
fn create_first_with_retry(endpoint: &str) -> io::Result<NamedPipeServer> {
    let mut delay = Duration::from_millis(20); let mut last_error = None;
    for attempt in 0..FIRST_BIND_ATTEMPTS {
        match create_server(endpoint, true) { Ok(pipe) => return Ok(pipe), Err(error) => last_error = Some(error) }
        if attempt + 1 < FIRST_BIND_ATTEMPTS { std::thread::sleep(delay); delay = (delay * 2).min(Duration::from_millis(160)); }
    }
    Err(last_error.expect("first bind attempts is nonzero"))
}
async fn create_with_retry(endpoint: &str) -> io::Result<NamedPipeServer> {
    let mut delay = Duration::from_millis(5); let mut last_error = None;
    for attempt in 0..REPLACEMENT_ATTEMPTS {
        let native = endpoint.to_owned();
        let created = tokio::task::spawn_blocking(move || create_server(&native, false))
            .await
            .map_err(|error| io::Error::other(format!("pipe create worker failed: {error}")))?;
        match created { Ok(pipe) => return Ok(pipe), Err(error) => last_error = Some(error) }
        if attempt + 1 < REPLACEMENT_ATTEMPTS { tokio::time::sleep(delay).await; delay = (delay * 2).min(Duration::from_millis(80)); }
    }
    Err(last_error.expect("replacement attempts is nonzero"))
}

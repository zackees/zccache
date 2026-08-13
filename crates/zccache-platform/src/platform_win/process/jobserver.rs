pub fn is_supported() -> bool {
    false
}

#[derive(Debug)]
pub struct NativeJobserver;

impl NativeJobserver {
    pub fn create(_capacity: usize) -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "GNU make jobserver pipes are unavailable on Windows",
        ))
    }

    pub fn auth_string(&self) -> String {
        unreachable!("Windows cannot construct a native jobserver")
    }
}

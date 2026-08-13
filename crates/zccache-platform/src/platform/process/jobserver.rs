//! Native jobserver primitives.

#[must_use]
pub fn is_supported() -> bool {
    crate::platform_imp::process::jobserver::is_supported()
}

/// Host-owned GNU make jobserver primitive.
#[derive(Debug)]
pub struct NativeJobserver {
    inner: crate::platform_imp::process::jobserver::NativeJobserver,
}

impl NativeJobserver {
    pub fn create(capacity: usize) -> std::io::Result<Self> {
        if capacity == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "jobserver capacity must be greater than zero",
            ));
        }
        crate::platform_imp::process::jobserver::NativeJobserver::create(capacity)
            .map(|inner| Self { inner })
    }

    #[must_use]
    pub fn auth_string(&self) -> String {
        self.inner.auth_string()
    }
}

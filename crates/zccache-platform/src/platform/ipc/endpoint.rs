use std::fmt;

use crate::platform_imp;

/// Opaque native local-IPC endpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Endpoint(pub(crate) platform_imp::ipc::Endpoint);

impl Endpoint {
    /// Construct from a product-owned native endpoint string.
    pub fn from_native(value: impl Into<String>) -> Self {
        Self(platform_imp::ipc::Endpoint::from_native(value.into()))
    }

    /// Native string accepted by the host transport.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Retire a stale endpoint before binding.
    pub fn retire(&self) -> std::io::Result<()> {
        self.0.retire()
    }

    /// Collision-resistant endpoint for local tests.
    pub fn unique_test(name: &str) -> Self {
        Self(platform_imp::ipc::Endpoint::unique_test(name))
    }

    /// Select the native representation from product-owned file and pipe names.
    pub fn select(file_path: impl Into<String>, pipe_name: impl Into<String>) -> Self {
        Self(platform_imp::ipc::Endpoint::select(
            file_path.into(),
            pipe_name.into(),
        ))
    }

    /// Convert product endpoint text to running-process local-socket text.
    pub fn to_running_process(&self) -> String {
        self.0.to_running_process()
    }

    /// Convert running-process local-socket text to native endpoint text.
    pub fn from_running_process(value: impl Into<String>) -> Self {
        Self(platform_imp::ipc::Endpoint::from_running_process(
            value.into(),
        ))
    }

    /// Whether a file-path representation fits the portable socket limit.
    pub fn file_path_is_portable(value: &str) -> bool {
        platform_imp::ipc::Endpoint::file_path_is_portable(value)
    }

    /// Whether this host represents the endpoint as a filesystem path.
    pub fn uses_file_path(&self) -> bool {
        self.0.uses_file_path()
    }

    /// Existing product connection deadline for this host transport.
    pub fn connect_timeout(&self) -> std::time::Duration {
        self.0.connect_timeout()
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

use crate::platform_imp;

/// Opaque peer process/user identity facts.
pub struct PeerIdentity(pub(crate) platform_imp::ipc::PeerIdentity);

impl PeerIdentity {
    /// Native process id when the transport exposes one.
    pub fn pid(&self) -> Option<u32> {
        self.0.pid()
    }

    /// Whether the kernel authenticated this peer as the current user.
    pub fn is_current_user(&self) -> bool {
        self.0.is_current_user()
    }

    /// Stable product-facing rejection reason, if authentication failed.
    pub fn rejection_reason(&self) -> Option<&'static str> {
        self.0.rejection_reason()
    }
}

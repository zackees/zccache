pub struct PeerIdentity { pub(super) pid: Option<u32> }
impl PeerIdentity {
    pub fn pid(&self) -> Option<u32> { self.pid }
    pub fn is_current_user(&self) -> bool { true }
    pub fn rejection_reason(&self) -> Option<&'static str> { None }
}

pub struct PeerIdentity { pub(super) pid: Option<u32>, pub(super) current_user: bool, pub(super) credentials_available: bool }
impl PeerIdentity {
    pub fn pid(&self) -> Option<u32> { self.pid }
    pub fn is_current_user(&self) -> bool { self.current_user }
    pub fn rejection_reason(&self) -> Option<&'static str> {
        if !self.credentials_available { Some("peer-cred-unavailable") }
        else if !self.current_user { Some("foreign-uid") } else { None }
    }
}

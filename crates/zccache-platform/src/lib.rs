//! The one place in the workspace that selects the host platform.
//!
//! Every other zccache crate consumes the neutral facades re-exported here
//! (`crate::platform::{process, fs, ipc, executable, host}`) and never names
//! a concrete implementation. There is deliberately no fallback arm: an
//! unsupported host OS fails compilation at this selector instead of
//! receiving a partial generic implementation. See the crate README and
//! `docs/architecture/portability.md` for the full boundary contract.

use std::cfg_select;

mod platform;
pub use platform::{executable, fs, host, ipc, process};

// The alias is consumed by the neutral facades as each capability phase
// fills in; until then the selector itself is the only host code, and each
// arm's alias is allowed to be unused.
cfg_select! {
    target_os = "windows" => {
        mod platform_win;
        #[allow(unused_imports)]
        pub(crate) use platform_win as platform_imp;
    }
    target_os = "linux" => {
        mod platform_linux;
        #[allow(unused_imports)]
        pub(crate) use platform_linux as platform_imp;
    }
    target_os = "macos" => {
        mod platform_macos;
        #[allow(unused_imports)]
        pub(crate) use platform_macos as platform_imp;
    }
}

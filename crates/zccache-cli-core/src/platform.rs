//! CLI-owned product policy and canonical kernal-api capability aliases.
//!
//! The module intentionally retains the internal `platform` spelling while
//! the standalone platform crate is removed. Neutral capabilities are direct
//! re-exports, so their type identity and behavior remain kernal-api's.

#[cfg(feature = "cli")]
pub(crate) mod executable {
    pub(crate) use kernal_api::platform::executable::{
        current_image, file_name_os as native_name, find_on_path, stem_matches,
    };
}

pub(crate) mod fs {
    pub(crate) mod identity {
        #[cfg(any(feature = "cli", test))]
        pub(crate) use kernal_api::platform::fs::path_file::{file_identity, FileIdentity};
    }

    #[cfg(feature = "cli")]
    pub(crate) mod permissions {
        pub(crate) use kernal_api::platform::fs::make_executable;
    }

    #[cfg(feature = "cli")]
    pub(crate) mod replace {
        pub(crate) use kernal_api::platform::fs::replacement::is_lock_contention;
    }
}

#[cfg(feature = "cli")]
pub(crate) mod host {
    pub(crate) use zccache_core::host::{is_windows, os};

    pub(crate) fn defender_supported() -> bool {
        is_windows()
    }
}

#[cfg(any(feature = "cli", test))]
pub(crate) mod process {
    pub(crate) mod spawn {
        #[cfg(not(windows))]
        pub(crate) fn run_cli_entry(
            entry: fn() -> std::process::ExitCode,
        ) -> std::process::ExitCode {
            entry()
        }

        #[cfg(windows)]
        pub(crate) fn run_cli_entry(
            entry: fn() -> std::process::ExitCode,
        ) -> std::process::ExitCode {
            match std::thread::Builder::new()
                .name("zccache-cli".to_string())
                .stack_size(8 * 1024 * 1024)
                .spawn(entry)
            {
                Ok(handle) => handle.join().unwrap_or(std::process::ExitCode::FAILURE),
                Err(error) => {
                    eprintln!("zccache: failed to start CLI thread: {error}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_aliases_retain_kernal_api_type_identity() {
        let _: fn(
            &std::path::Path,
        ) -> std::io::Result<kernal_api::platform::fs::path_file::FileIdentity> =
            fs::identity::file_identity;
        fn to_kernal_api(
            value: fs::identity::FileIdentity,
        ) -> kernal_api::platform::fs::path_file::FileIdentity {
            value
        }
        fn from_kernal_api(
            value: kernal_api::platform::fs::path_file::FileIdentity,
        ) -> fs::identity::FileIdentity {
            value
        }
        let _ = (to_kernal_api, from_kernal_api);
    }

    #[test]
    fn cli_entry_preserves_success_exit_code() {
        assert_eq!(
            process::spawn::run_cli_entry(|| std::process::ExitCode::SUCCESS),
            std::process::ExitCode::SUCCESS
        );
    }
}

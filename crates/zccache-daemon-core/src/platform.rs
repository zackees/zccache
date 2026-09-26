//! Daemon-owned policy adapters over canonical kernal-api capabilities.
//!
//! This intentionally replaces the former standalone platform crate without
//! recreating substrate-owned types. The two small adapters below are zccache
//! response/materialization compatibility policy; every native operation is a
//! direct kernal-api alias or delegation.

pub(crate) mod executable {
    pub(crate) use kernal_api::platform::executable::{current_image, unlock_for_replacement};
}

pub(crate) mod fs {
    pub(crate) use kernal_api::platform::fs::FileChangeMarker as ChangeMarker;

    pub(crate) use identity::FileIdentity;

    pub(crate) mod durability {
        pub(crate) use kernal_api::platform::fs::{
            open_shared_append, sync_directory_if_supported as sync_directory,
        };
    }

    pub(crate) mod identity {
        pub(crate) use kernal_api::platform::fs::{
            file_change_marker as change_marker,
            path_file::{file_identity, same_file, FileIdentity},
        };
    }

    pub(crate) mod links {
        #[cfg(test)]
        pub(crate) use kernal_api::platform::fs::symlink_file;
        pub(crate) use kernal_api::platform::fs::{classify, hard_link_count, LinkKind};
    }

    pub(crate) mod path {
        pub(crate) use zccache_core::path::strip_verbatim_prefix;
    }

    pub(crate) mod writers {
        pub(crate) use kernal_api::platform::fs::{await_no_writers, WriterWait};
    }

    pub(crate) mod permissions {
        #[cfg(test)]
        pub(crate) use kernal_api::platform::fs::make_executable;
        pub(crate) use kernal_api::platform::fs::{
            apply_metadata_mode as apply_mode, metadata_mode as mode, set_readonly,
        };

        /// zccache materialization policy: a dangling link is removable, and
        /// a missing Windows destination is historically a no-op.
        pub(crate) fn make_writable(path: &std::path::Path) -> std::io::Result<()> {
            match set_readonly(path, false) {
                Ok(()) => Ok(()),
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        && (kernal_api::platform::host::target_is_windows()
                            || std::fs::symlink_metadata(path)
                                .map(|metadata| metadata.file_type().is_symlink())
                                .unwrap_or(false)) =>
                {
                    Ok(())
                }
                Err(error) => Err(error),
            }
        }
    }

    pub(crate) mod replace {
        pub(crate) use kernal_api::platform::fs::replacement::{
            atomic_replace, install_directory, rename_generation as rename_without_replace,
            replace_with_delete_fallback,
        };
    }

    pub(crate) mod volume {
        pub(crate) use kernal_api::platform::fs::{
            allocated_bytes, file_id_width, volume_identity_u128,
        };
    }
}

pub(crate) mod host {
    pub(crate) use zccache_core::host::{
        arch, available_parallelism, home_dir, is_linux, is_macos, is_windows, os,
    };
}

pub(crate) mod process {
    pub(crate) mod exit {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub(crate) struct ExitOutcome {
            pub(crate) exit_code: i32,
            pub(crate) termination_signal: Option<i32>,
        }

        pub(crate) fn outcome(status: &std::process::ExitStatus) -> ExitOutcome {
            let trampoline_code = kernal_api::platform::process::trampoline_exit_code(*status);
            let termination_signal = (!kernal_api::platform::host::target_is_windows())
                .then(|| {
                    trampoline_code
                        .checked_sub(128)
                        .filter(|signal| (1..=127).contains(signal))
                })
                .flatten();
            ExitOutcome {
                exit_code: termination_signal.map_or(trampoline_code, |signal| -128 - signal),
                termination_signal,
            }
        }

        pub(crate) fn termination_signal_from_exit_code(exit_code: i32) -> Option<i32> {
            (!kernal_api::platform::host::target_is_windows())
                .then(|| exit_code.checked_neg()?.checked_sub(128))
                .flatten()
                .filter(|signal| (1..=127).contains(signal))
        }
    }

    pub(crate) mod inspect {
        pub(crate) use kernal_api::platform::process::PEAK_RSS_READABLE_AFTER_EXIT;

        #[cfg(test)]
        pub(crate) fn is_alive(pid: u32) -> bool {
            kernal_api::platform::process::ProcessLiveness::open(pid)
                .is_ok_and(|handle| handle.is_alive())
        }

        #[cfg(test)]
        pub(crate) fn cpu_ticks(pid: u32) -> Option<u64> {
            kernal_api::platform::process::cpu_ticks_for_pid(pid)
        }

        /// Resident-memory high-water mark of `pid` (zccache#1586).
        pub(crate) fn peak_rss_bytes(pid: u32) -> Option<u64> {
            kernal_api::platform::process::peak_rss_bytes_for_pid(pid)
        }

        /// Current resident bytes of `pid` and its live descendants
        /// (zccache#1588), bounded by
        /// `kernal_api::platform::process::MAX_TREE_RSS_PROCESSES`.
        pub(crate) fn tree_rss_bytes(pid: u32) -> Option<u64> {
            kernal_api::platform::process::tree_rss_bytes_for_pid(pid)
        }

        /// CPU ticks burned by the live descendants of `pid`, not by `pid`
        /// itself. A compiler driver such as gcc waits while its `cc1plus`
        /// child does the work, so only the tree shows that progress.
        /// `None` where descendants cannot be enumerated.
        #[cfg(target_os = "linux")]
        pub(crate) fn descendant_cpu_ticks(pid: u32) -> Option<u64> {
            let mut total = 0u64;
            let mut seen = std::collections::HashSet::from([pid]);
            let mut stack = vec![pid];
            while let Some(parent) = stack.pop() {
                let Ok(tasks) = std::fs::read_dir(format!("/proc/{parent}/task")) else {
                    continue;
                };
                for task in tasks.flatten() {
                    let Ok(children) = std::fs::read_to_string(task.path().join("children")) else {
                        continue;
                    };
                    for child in children
                        .split_whitespace()
                        .filter_map(|value| value.parse::<u32>().ok())
                    {
                        if seen.len() >= kernal_api::platform::process::MAX_TREE_RSS_PROCESSES
                            || !seen.insert(child)
                        {
                            continue;
                        }
                        if let Some(ticks) = kernal_api::platform::process::cpu_ticks_for_pid(child)
                        {
                            total = total.wrapping_add(ticks);
                        }
                        stack.push(child);
                    }
                }
            }
            std::path::Path::new(&format!("/proc/{pid}"))
                .exists()
                .then_some(total)
        }

        #[cfg(not(target_os = "linux"))]
        pub(crate) fn descendant_cpu_ticks(_pid: u32) -> Option<u64> {
            None
        }
    }

    pub(crate) mod jobserver {
        pub(crate) use kernal_api::platform::process::NativeJobserver;

        #[cfg(test)]
        pub(crate) fn is_supported() -> bool {
            kernal_api::platform::process::native_jobserver_supported()
        }
    }

    pub(crate) mod stdio {
        pub(crate) use kernal_api::platform::process::{
            detach_standard_streams as detach, redirect_standard_streams_to_log as redirect_to_log,
        };
    }

    #[cfg(test)]
    pub(crate) mod terminate {
        /// Best-effort, generation-safe force kill for test cleanup.
        pub(crate) fn force(pid: u32) {
            use kernal_api::platform::process::{capture_identity, ProcessIdentityCapture};
            if let ProcessIdentityCapture::Found(identity) = capture_identity(pid) {
                let _ = kernal_api::platform::process::force_kill(identity);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_filesystem_aliases_preserve_type_identity() {
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
}

//! Packaged `running-process` service definition for the zccache daemon.
//!
//! Discovery slice of the running-process adoption
//! (zackees/running-process#383, following the soldr#722 pattern): writes
//! a `zccache.servicedef` protobuf into the running-process service
//! definition directory so the shared broker can discover and verify the
//! zccache daemon. This does not change how the daemon is spawned or how
//! clients connect — the broker connect lane stays opt-in
//! (`ZCCACHE_BROKER_CONNECT=1`, see `crate::ipc::broker`).
//!
//! ## v2 dual-write (slice 21 of zccache#782)
//!
//! As of running-process PR #522 a v2 `.servicedef.v2` write path
//! exists upstream (`broker::protocol_v2::ServiceDefinitionBuilder` +
//! `write_service_definition_v2`). To stay reachable from *both* v1
//! and v2 brokers during the rollout, this module now writes BOTH:
//!
//! - `zccache.servicedef` — v1 protobuf, primary, read by today's
//!   v1 broker. STAYS until v2 broker has a loader (separate
//!   running-process slice) AND zccache's `ipc/broker.rs` migrates
//!   off v1's `client::*` surface (zccache#782 slice 25).
//! - `zccache.servicedef.v2` — v2 protobuf, secondary, read by the
//!   v2 broker scaffold (loader still in flight). Carries the same
//!   semantic identity (\"shared broker, pinned to this binary +
//!   version range\") so a v2 broker finds zccache at the same
//!   identity v1 brokers know.
//!
//! When zccache flips slice 25 (delete v1 broker surface) the v1
//! write goes away with it.

use running_process::broker::builders::ServiceDefinitionBuilder;
use running_process::broker::protocol::ServiceDefinition;
use running_process::broker::protocol_v2;
use running_process::broker::server::service_definition_dir;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Service name the daemon already answers `BackendHandle` probes for
/// (see `crate::ipc::probe_backend_handle`).
pub(crate) const ZCCACHE_SERVICE_NAME: &str = "zccache";

/// An installed service definition: where it was written and what it says.
///
/// `path` is the v1 file location (primary during the v1→v2 rollout).
/// `v2_path` is the v2 file location (written alongside via dual-write
/// per the module docstring); `None` indicates the v2 write was
/// skipped or failed without aborting the v1 install — the rollout
/// keeps v1 primary so a v2 write failure does not block daemon
/// startup. zccache#782 slice 25 will collapse this back to a single
/// path once v1 goes away.
#[derive(Debug, Clone)]
pub(crate) struct InstalledServiceDefinition {
    pub(crate) path: PathBuf,
    pub(crate) definition: ServiceDefinition,
    pub(crate) v2_path: Option<PathBuf>,
}

/// Build the zccache daemon's v2 ServiceDefinition counterpart.
///
/// Mirrors [`zccache_service_definition`]'s v1 shape using the v2
/// `protocol_v2::ServiceDefinitionBuilder`. The two share an identity
/// (service_name + binary_path + isolation + version pin + labels) so
/// a v2 broker discovers the same zccache daemon a v1 broker would.
fn zccache_service_definition_v2(
    daemon_binary: &Path,
) -> io::Result<protocol_v2::ServiceDefinition> {
    let binary = std::fs::canonicalize(daemon_binary)?;
    let binary_dir = binary.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "zccache-daemon binary path has no parent directory",
        )
    })?;

    Ok(
        protocol_v2::ServiceDefinitionBuilder::shared_broker(
            ZCCACHE_SERVICE_NAME,
            binary.display().to_string(),
        )
        .per_version_binary_dir(binary_dir.display().to_string())
        .min_version(crate::core::VERSION)
        .version_allow_list([crate::core::VERSION])
        .label("vendor", "zackees")
        .label("package", "zccache")
        .label("consumer", "zccache")
        .label("running-process-tracker", "zackees/running-process#435")
        .build(),
    )
}

/// Build the zccache daemon service definition for the given daemon binary.
///
/// Uses the frozen `ServiceDefinitionBuilder` (zackees/running-process#433):
/// it defaults the broker-owned boilerplate, validates the absolute binary
/// path, and produces the same `SHARED_BROKER` definition the daemon answers
/// `BackendHandle` probes for.
pub(crate) fn zccache_service_definition(daemon_binary: &Path) -> io::Result<ServiceDefinition> {
    let binary = std::fs::canonicalize(daemon_binary)?;
    let binary_dir = binary.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "zccache-daemon binary path has no parent directory",
        )
    })?;

    ServiceDefinitionBuilder::shared_broker(ZCCACHE_SERVICE_NAME, binary.display().to_string())
        .per_version_binary_dir(binary_dir.display().to_string())
        .min_version(crate::core::VERSION)
        .allow_version(crate::core::VERSION)
        .label("vendor", "zackees")
        .label("package", "zccache")
        .label("consumer", "zccache")
        .label("running-process-tracker", "zackees/running-process#435")
        .build()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))
}

/// Install the service definition into the default running-process
/// service-definition directory.
pub(crate) fn install_service_definition(
    daemon_binary: &Path,
) -> io::Result<InstalledServiceDefinition> {
    install_service_definition_to_dir(service_definition_dir(), daemon_binary)
}

/// Install the service definition into an explicit directory (used by the
/// `--dir` override and tests).
pub(crate) fn install_service_definition_to_dir(
    service_root: impl AsRef<Path>,
    daemon_binary: &Path,
) -> io::Result<InstalledServiceDefinition> {
    let service_root = service_root.as_ref();
    let binary = std::fs::canonicalize(daemon_binary)?;
    let binary_dir = binary.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "zccache-daemon binary path has no parent directory",
        )
    })?;

    // The builder validates and writes the `.servicedef` atomically into the
    // service-definition root. Build the definition once for the return value
    // and once through `install_in` so the persisted bytes are loader-compatible.
    let definition = zccache_service_definition(daemon_binary)?;
    let path =
        ServiceDefinitionBuilder::shared_broker(ZCCACHE_SERVICE_NAME, binary.display().to_string())
            .per_version_binary_dir(binary_dir.display().to_string())
            .min_version(crate::core::VERSION)
            .allow_version(crate::core::VERSION)
            .label("vendor", "zackees")
            .label("package", "zccache")
            .label("consumer", "zccache")
            .label("running-process-tracker", "zackees/running-process#435")
            .install_in(service_root)
            .map_err(|err| io::Error::other(err.to_string()))?;

    // Dual-write: also install the v2 ServiceDefinition counterpart so a
    // v2 broker (with loader) finds zccache at the same identity. v2
    // failures are reported but do NOT abort the v1 install — v1 is
    // primary during the rollout. zccache#782 slice 25 collapses this
    // to v2-only.
    let v2_path = match zccache_service_definition_v2(daemon_binary) {
        Ok(def_v2) => match protocol_v2::write_service_definition_v2(service_root, &def_v2) {
            Ok(path) => Some(path),
            Err(err) => {
                tracing::warn!(
                    "v2 ServiceDefinition install failed (non-fatal during rollout): {err}"
                );
                None
            }
        },
        Err(err) => {
            tracing::warn!("v2 ServiceDefinition build failed (non-fatal during rollout): {err}");
            None
        }
    };

    Ok(InstalledServiceDefinition {
        path,
        definition,
        v2_path,
    })
}

/// `zccache install-servicedef [--daemon-binary <path>] [--dir <path>]`
pub(crate) fn run_install_servicedef(
    daemon_binary: Option<String>,
    dir: Option<String>,
) -> ExitCode {
    let daemon_binary = match daemon_binary {
        Some(path) => PathBuf::from(path),
        None => match super::daemon::find_daemon_binary() {
            Some(path) => path.into_path_buf(),
            None => {
                eprintln!(
                    "zccache: cannot find zccache-daemon binary; pass --daemon-binary <path>"
                );
                return ExitCode::FAILURE;
            }
        },
    };

    let installed = match dir {
        Some(dir) => install_service_definition_to_dir(dir, &daemon_binary),
        None => install_service_definition(&daemon_binary),
    };
    match installed {
        Ok(installed) => {
            println!(
                "installed {} (service `{}`, daemon `{}`)",
                installed.path.display(),
                installed.definition.service_name,
                installed.definition.binary_path,
            );
            match &installed.v2_path {
                Some(v2_path) => {
                    println!("dual-write v2: {}", v2_path.display());
                }
                None => {
                    println!("dual-write v2: skipped (see daemon log for cause)");
                }
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("zccache: failed to install service definition: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use running_process::broker::protocol::BrokerIsolation;
    use running_process::broker::server::ServiceDefinitionLoader;
    use tempfile::TempDir;

    fn fake_daemon_binary(root: &Path) -> PathBuf {
        let binary = root.join(if cfg!(windows) {
            "zccache-daemon.exe"
        } else {
            "zccache-daemon"
        });
        std::fs::write(&binary, b"stub").expect("fake daemon binary");
        binary
    }

    #[test]
    fn service_definition_declares_zccache_shared_broker() {
        let temp = TempDir::new().expect("tempdir");
        let daemon = fake_daemon_binary(temp.path());

        let definition = zccache_service_definition(&daemon).expect("definition");

        assert_eq!(definition.service_name, "zccache");
        assert_eq!(definition.isolation, BrokerIsolation::SharedBroker as i32);
        assert_eq!(definition.min_version, crate::core::VERSION);
        assert_eq!(definition.version_allow_list, [crate::core::VERSION]);
        assert_eq!(
            definition
                .labels
                .get("running-process-tracker")
                .map(String::as_str),
            Some("zackees/running-process#435"),
        );
        assert_eq!(
            definition.labels.get("consumer").map(String::as_str),
            Some("zccache"),
        );
    }

    #[test]
    fn install_writes_loader_compatible_protobuf() {
        let temp = TempDir::new().expect("tempdir");
        let service_root = temp.path().join("services");
        let daemon = fake_daemon_binary(temp.path());

        let installed = install_service_definition_to_dir(&service_root, &daemon)
            .expect("install service definition");

        assert_eq!(installed.path, service_root.join("zccache.servicedef"));
        let loaded = ServiceDefinitionLoader::new(&service_root)
            .load("zccache")
            .expect("load service definition");
        assert_eq!(loaded, installed.definition);
    }

    /// Slice 21 of zccache#782: dual-write also produces a v2
    /// `zccache.servicedef.v2` file alongside the v1 file, carrying
    /// the same service_name + binary_path + isolation + version pin.
    /// Pins that a v2 broker would discover the same zccache identity
    /// once it has a loader.
    #[test]
    fn install_also_writes_v2_servicedef() {
        use prost::Message;

        let temp = TempDir::new().expect("tempdir");
        let service_root = temp.path().join("services");
        let daemon = fake_daemon_binary(temp.path());

        let installed = install_service_definition_to_dir(&service_root, &daemon)
            .expect("install service definition");

        let v2_path = installed.v2_path.expect("v2 path must be present");
        assert_eq!(v2_path, service_root.join("zccache.servicedef.v2"));

        let bytes = std::fs::read(&v2_path).expect("read v2 file");
        let decoded = protocol_v2::ServiceDefinition::decode(bytes.as_slice())
            .expect("v2 ServiceDefinition decodes");

        assert_eq!(decoded.service_name, "zccache");
        assert_eq!(decoded.binary_path, installed.definition.binary_path);
        assert_eq!(
            decoded.isolation,
            protocol_v2::BrokerIsolation::SharedBroker as i32,
            "v2 must mirror v1's shared_broker isolation"
        );
        assert_eq!(decoded.min_version, crate::core::VERSION);
        assert_eq!(decoded.version_allow_list, vec![crate::core::VERSION]);
        assert_eq!(
            decoded.labels.get("consumer").map(String::as_str),
            Some("zccache")
        );
        assert_eq!(
            decoded
                .labels
                .get("running-process-tracker")
                .map(String::as_str),
            Some("zackees/running-process#435")
        );
    }

    #[test]
    fn service_name_matches_backend_handle_probe() {
        // The servicedef must advertise the same service name the daemon
        // answers BackendHandle probes for (ipc::probe_backend_handle).
        assert_eq!(ZCCACHE_SERVICE_NAME, "zccache");
    }
}

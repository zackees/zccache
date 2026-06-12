//! Packaged `running-process` service definition for the zccache daemon.
//!
//! Discovery slice of the running-process adoption
//! (zackees/running-process#383, following the soldr#722 pattern): writes
//! a `zccache.servicedef` protobuf into the running-process service
//! definition directory so the shared broker can discover and verify the
//! zccache daemon. This does not change how the daemon is spawned or how
//! clients connect — the broker connect lane stays opt-in
//! (`ZCCACHE_BROKER_CONNECT=1`, see `crate::ipc::broker`).

use prost::Message as _;
use running_process::broker::protocol::{BrokerIsolation, ServiceDefinition};
use running_process::broker::server::{
    ensure_service_definition_dir, service_definition_dir, service_definition_path,
    validate_service_definition_for_service,
};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Service name the daemon already answers `BackendHandle` probes for
/// (see `crate::ipc::probe_backend_handle`).
pub(crate) const ZCCACHE_SERVICE_NAME: &str = "zccache";

/// An installed service definition: where it was written and what it says.
#[derive(Debug, Clone)]
pub(crate) struct InstalledServiceDefinition {
    pub(crate) path: PathBuf,
    pub(crate) definition: ServiceDefinition,
}

/// Build the zccache daemon service definition for the given daemon binary.
pub(crate) fn zccache_service_definition(daemon_binary: &Path) -> io::Result<ServiceDefinition> {
    let binary = std::fs::canonicalize(daemon_binary)?;
    let binary_dir = binary.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "zccache-daemon binary path has no parent directory",
        )
    })?;

    let mut labels = HashMap::new();
    labels.insert("vendor".to_string(), "zackees".to_string());
    labels.insert("package".to_string(), "zccache".to_string());
    labels.insert(
        "running-process-tracker".to_string(),
        "zackees/running-process#383".to_string(),
    );

    let definition = ServiceDefinition {
        service_name: ZCCACHE_SERVICE_NAME.to_string(),
        binary_path: binary.display().to_string(),
        isolation: BrokerIsolation::SharedBroker as i32,
        explicit_instance: String::new(),
        per_version_binary_dir: binary_dir.display().to_string(),
        min_version: crate::core::VERSION.to_string(),
        version_allow_list: vec![crate::core::VERSION.to_string()],
        labels,
    };
    validate_service_definition_for_service(&definition, ZCCACHE_SERVICE_NAME)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
    Ok(definition)
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
    ensure_service_definition_dir(service_root).map_err(|err| io::Error::other(err.to_string()))?;
    let definition = zccache_service_definition(daemon_binary)?;
    let path = service_definition_path(service_root, ZCCACHE_SERVICE_NAME)
        .map_err(|err| io::Error::other(err.to_string()))?;

    std::fs::write(&path, definition.encode_to_vec())?;
    Ok(InstalledServiceDefinition { path, definition })
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
            Some("zackees/running-process#383"),
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

    #[test]
    fn service_name_matches_backend_handle_probe() {
        // The servicedef must advertise the same service name the daemon
        // answers BackendHandle probes for (ipc::probe_backend_handle).
        assert_eq!(ZCCACHE_SERVICE_NAME, "zccache");
    }
}

//! zccache-owned host policy shared by core configuration and diagnostics.
//!
//! This module intentionally contains product policy only: environment-first
//! home selection, Defender PowerShell behavior, and legacy non-Windows
//! elevation semantics. Native target and elevation observations delegate to
//! kernal-api.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub use kernal_api::platform::host::{
    available_parallelism, target_is_linux as is_linux, target_is_macos as is_macos,
    target_is_windows as is_windows,
};

pub fn os() -> &'static str {
    kernal_api::platform::host::process_target().os
}

pub fn arch() -> &'static str {
    kernal_api::platform::host::process_target().architecture
}

/// Preserve zccache's environment-first home namespace selection.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| {
            is_windows()
                .then(|| std::env::var_os("USERPROFILE"))
                .flatten()
        })
        .map(PathBuf::from)
}

/// Opaque raw host inputs for native-CPU cache-key salting.
///
/// Labels, ordering, and the PID fallback are zccache cache-key
/// compatibility policy. Callers must hash the result before persistence.
#[must_use]
pub fn cpu_identity_material() -> String {
    let target_os = os();
    let (machine_id, host_label, hostname) = match target_os {
        "linux" => (
            std::fs::read_to_string("/etc/machine-id")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(|value| value.trim().to_owned()),
            "HOSTNAME",
            std::env::var_os("HOSTNAME"),
        ),
        "macos" => (None, "HOSTNAME", std::env::var_os("HOSTNAME")),
        "windows" => (None, "COMPUTERNAME", std::env::var_os("COMPUTERNAME")),
        _ => (None, "HOSTNAME", std::env::var_os("HOSTNAME")),
    };
    let features = kernal_api::platform::host::cpu_compatibility_features();
    cpu_identity_material_from_parts(
        arch(),
        target_os,
        machine_id.as_deref(),
        host_label,
        hostname.as_deref(),
        std::process::id(),
        &features,
    )
}

fn cpu_identity_material_from_parts(
    architecture: &str,
    target_os: &str,
    machine_id: Option<&str>,
    host_label: &str,
    hostname: Option<&OsStr>,
    process_id: u32,
    features: &[&str],
) -> String {
    let mut material = format!("arch={architecture}\0os={target_os}");
    if let Some(machine_id) = machine_id.filter(|value| !value.is_empty()) {
        material.push_str("\0machine-id=");
        material.push_str(machine_id);
    }
    if let Some(hostname) = hostname {
        material.push('\0');
        material.push_str(host_label);
        material.push('=');
        material.push_str(&hostname.to_string_lossy());
    } else if machine_id.is_none_or(str::is_empty) {
        material.push_str("\0pid=");
        material.push_str(&process_id.to_string());
    }
    for feature in features {
        material.push_str("\0feature=");
        material.push_str(feature);
    }
    material
}

/// Preserve the legacy Defender exemption off Windows.
pub fn is_elevated() -> bool {
    if is_windows() {
        kernal_api::platform::host::is_elevated().unwrap_or(false)
    } else {
        true
    }
}

#[derive(Debug)]
pub enum DefenderError {
    Unsupported,
    PowerShellNotFound,
    CommandFailed {
        exit_code: Option<i32>,
        stderr: String,
    },
    OutputParse(String),
    Io(std::io::Error),
}

pub fn defender_exclusions() -> Result<Vec<PathBuf>, DefenderError> {
    if !is_windows() {
        return Err(DefenderError::Unsupported);
    }
    let raw = run_powershell(&[
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "(Get-MpPreference).ExclusionPath",
    ])?;
    Ok(raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect())
}

pub fn add_defender_exclusion(path: &Path) -> Result<(), DefenderError> {
    mutate_defender("Add-MpPreference", path)
}

pub fn remove_defender_exclusion(path: &Path) -> Result<(), DefenderError> {
    mutate_defender("Remove-MpPreference", path)
}

fn mutate_defender(command: &str, path: &Path) -> Result<(), DefenderError> {
    if !is_windows() {
        return Err(DefenderError::Unsupported);
    }
    let quoted = format!("'{}'", path.to_string_lossy().replace('\'', "''"));
    run_powershell(&[
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        &format!("{command} -ExclusionPath {quoted}"),
    ])?;
    Ok(())
}

fn run_powershell(args: &[&str]) -> Result<String, DefenderError> {
    let mut command = std::process::Command::new("powershell.exe");
    command.args(args);
    let output = kernal_api::platform::process::foreground_output(&mut command)
        .map_err(map_powershell_error)?;
    if !output.status.success() {
        return Err(DefenderError::CommandFailed {
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    String::from_utf8(output.stdout).map_err(|error| DefenderError::OutputParse(error.to_string()))
}

fn map_powershell_error(error: std::io::Error) -> DefenderError {
    if error.kind() == std::io::ErrorKind::NotFound {
        DefenderError::PowerShellNotFound
    } else {
        DefenderError::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_identity_material_retains_labels_order_and_fallbacks() {
        let material = cpu_identity_material_from_parts(
            "x86_64",
            "linux",
            Some("machine"),
            "HOSTNAME",
            Some(OsStr::new("builder")),
            42,
            &["sse2", "avx2"],
        );
        assert_eq!(
            material,
            "arch=x86_64\0os=linux\0machine-id=machine\0HOSTNAME=builder\0feature=sse2\0feature=avx2"
        );
        assert_eq!(
            cpu_identity_material_from_parts(
                "x86_64",
                "windows",
                None,
                "COMPUTERNAME",
                None,
                42,
                &[],
            ),
            "arch=x86_64\0os=windows\0pid=42"
        );
    }

    #[test]
    fn cpu_identity_material_is_present_and_stable_within_a_process() {
        assert!(!cpu_identity_material().is_empty());
        assert_eq!(cpu_identity_material(), cpu_identity_material());
    }
}

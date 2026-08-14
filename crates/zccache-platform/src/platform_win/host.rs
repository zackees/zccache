//! Windows host facts and privilege probes.

use std::path::PathBuf;

pub const IS_WINDOWS: bool = true;
pub const IS_MACOS: bool = false;
pub const IS_LINUX: bool = false;

pub const fn os() -> &'static str { std::env::consts::OS }
pub const fn arch() -> &'static str { std::env::consts::ARCH }

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

pub fn current_user() -> Option<String> {
    std::env::var("USERNAME").ok()
}

pub fn runtime_dir() -> Option<String> {
    None
}

pub fn cpu_identity_material() -> String {
    let mut material = format!("arch={}\0os={}", arch(), os());
    if let Some(name) = std::env::var_os("COMPUTERNAME") {
        material.push_str("\0COMPUTERNAME=");
        material.push_str(&name.to_string_lossy());
    } else {
        material.push_str("\0pid=");
        material.push_str(&std::process::id().to_string());
    }
    append_cpu_features(&mut material);
    material
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn append_cpu_features(material: &mut String) {
    for (name, present) in [
        ("sse2", std::arch::is_x86_feature_detected!("sse2")),
        ("sse4.2", std::arch::is_x86_feature_detected!("sse4.2")),
        ("avx", std::arch::is_x86_feature_detected!("avx")),
        ("avx2", std::arch::is_x86_feature_detected!("avx2")),
        ("avx512f", std::arch::is_x86_feature_detected!("avx512f")),
        ("fma", std::arch::is_x86_feature_detected!("fma")),
        ("bmi1", std::arch::is_x86_feature_detected!("bmi1")),
        ("bmi2", std::arch::is_x86_feature_detected!("bmi2")),
    ] {
        if present { material.push_str("\0feature="); material.push_str(name); }
    }
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
fn append_cpu_features(_: &mut String) {}

pub fn is_elevated() -> bool {
    use std::mem::size_of;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 { return false; }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut returned = 0;
        #[allow(clippy::cast_possible_truncation)]
        let ok = GetTokenInformation(token, TokenElevation, (&raw mut elevation).cast(), size_of::<TOKEN_ELEVATION>() as u32, &mut returned);
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

pub const fn defender_supported() -> bool {
    true
}

pub fn defender_exclusions() -> Result<Vec<PathBuf>, crate::host::DefenderError> {
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

pub fn add_defender_exclusion(path: &std::path::Path) -> Result<(), crate::host::DefenderError> {
    mutate_defender("Add-MpPreference", path)
}

pub fn remove_defender_exclusion(path: &std::path::Path) -> Result<(), crate::host::DefenderError> {
    mutate_defender("Remove-MpPreference", path)
}

fn mutate_defender(command: &str, path: &std::path::Path) -> Result<(), crate::host::DefenderError> {
    let quoted = format!("'{}'", path.to_string_lossy().replace('\'', "''"));
    run_powershell(&[
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        &format!("{command} -ExclusionPath {quoted}"),
    ])?;
    Ok(())
}

fn run_powershell(args: &[&str]) -> Result<String, crate::host::DefenderError> {
    let output = std::process::Command::new("powershell.exe")
        .args(args)
        .output()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                crate::host::DefenderError::PowerShellNotFound
            } else {
                crate::host::DefenderError::Io(error)
            }
        })?;
    if !output.status.success() {
        return Err(crate::host::DefenderError::CommandFailed {
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    String::from_utf8(output.stdout)
        .map_err(|error| crate::host::DefenderError::OutputParse(error.to_string()))
}

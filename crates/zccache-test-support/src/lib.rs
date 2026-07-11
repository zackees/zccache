//! Disposable real-filesystem fixtures with loud skip accounting (#1039).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[derive(Debug)]
pub struct FsFixture {
    name: &'static str,
    root: PathBuf,
    backing: Backing,
}

#[derive(Debug)]
enum Backing {
    Temp(tempfile::TempDir),
    #[cfg(windows)]
    WindowsVhd {
        temp: tempfile::TempDir,
        image: PathBuf,
    },
    #[cfg(windows)]
    WindowsSmb {
        temp: tempfile::TempDir,
        share: String,
    },
    #[cfg(target_os = "linux")]
    LinuxLoop {
        temp: tempfile::TempDir,
        device: String,
    },
    #[cfg(target_os = "macos")]
    MacImage {
        temp: tempfile::TempDir,
        mount: PathBuf,
    },
}

#[derive(Debug)]
pub struct FixtureSkip {
    pub name: &'static str,
    pub reason: String,
}

pub type FixtureResult = Result<FsFixture, FixtureSkip>;

impl FsFixture {
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn native(name: &'static str) -> FixtureResult {
        let temp = tempfile::Builder::new()
            .prefix("zccache-fs-native-")
            .tempdir()
            .map_err(|error| skip(name, format!("native tempdir failed: {error}")))?;
        Ok(Self {
            name,
            root: temp.path().to_path_buf(),
            backing: Backing::Temp(temp),
        })
    }

    pub fn refs_vhdx() -> FixtureResult {
        // A 512 MB ceiling was tried to reduce ReFS's metadata footprint
        // (its checksummed integrity streams scale with declared capacity
        // even under `format ... quick`) but empirically triggered VDS's
        // own "The volume size is too small" error — 1024 MB is below
        // that failure's boundary and is the size known to pass ReFS's
        // format step; the earlier intermittent "not enough space on the
        // disk" failures at this same 1024 MB size are CI-runner-level
        // VDS/diskpart flakiness (real free space was confirmed plentiful
        // — 32+ GB on C:, 143+ GB on D: — in a run that still failed),
        // mitigated by the retry loop wrapping this step in fs-matrix.yml
        // rather than by changing the declared size further.
        windows_vhd_sized("refs-vhdx", "refs", 1024)
    }

    pub fn fat32_vhdx() -> FixtureResult {
        windows_vhd("fat32-vhdx", "fat32")
    }

    pub fn exfat_vhdx() -> FixtureResult {
        windows_vhd("exfat-vhdx", "exfat")
    }

    pub fn second_volume_vhdx() -> FixtureResult {
        windows_vhd("cross-volume", "ntfs")
    }

    pub fn btrfs_loop() -> FixtureResult {
        linux_loop("btrfs-loop", "btrfs", "mkfs.btrfs")
    }

    pub fn ext4_loop() -> FixtureResult {
        linux_loop("ext4-loop", "ext4", "mkfs.ext4")
    }

    pub fn vfat_loop() -> FixtureResult {
        linux_loop("vfat-loop", "vfat", "mkfs.vfat")
    }

    pub fn tmpfs() -> FixtureResult {
        #[cfg(target_os = "linux")]
        {
            let root = Path::new("/dev/shm");
            if !root.is_dir() {
                return Err(skip("tmpfs", "/dev/shm is unavailable"));
            }
            let temp = tempfile::Builder::new()
                .prefix("zccache-tmpfs-")
                .tempdir_in(root)
                .map_err(|error| skip("tmpfs", format!("tempdir failed: {error}")))?;
            return Ok(Self {
                name: "tmpfs",
                root: temp.path().to_path_buf(),
                backing: Backing::Temp(temp),
            });
        }
        #[cfg(not(target_os = "linux"))]
        Err(skip("tmpfs", "tmpfs fixture is Linux-only"))
    }

    pub fn apfs_native() -> FixtureResult {
        #[cfg(target_os = "macos")]
        return Self::native("apfs-native");
        #[cfg(not(target_os = "macos"))]
        Err(skip("apfs-native", "APFS fixture is macOS-only"))
    }

    pub fn hfs_image() -> FixtureResult {
        mac_image("hfs-image", "HFS+")
    }

    pub fn exfat_image() -> FixtureResult {
        mac_image("exfat-image", "ExFAT")
    }

    pub fn nfs() -> FixtureResult {
        Err(skip(
            "nfs",
            "NFS needs an external server and remains best-effort per #1039",
        ))
    }

    pub fn smb_loopback() -> FixtureResult {
        #[cfg(windows)]
        {
            let temp = tempfile::Builder::new()
                .prefix("zccache-smb-")
                .tempdir()
                .map_err(|error| skip("smb-loopback", error.to_string()))?;
            let share = format!("zccache-fixture-{}", std::process::id());
            let spec = format!("{}={}", share, temp.path().display());
            let output = Command::new("net")
                .args(["share", &spec, "/GRANT:Everyone,FULL"])
                .output()
                .map_err(|error| skip("smb-loopback", format!("net share unavailable: {error}")))?;
            if !output.status.success() {
                return Err(skip("smb-loopback", command_error("net share", &output)));
            }
            let root = PathBuf::from(format!(r"\\localhost\{share}"));
            Ok(Self {
                name: "smb-loopback",
                root,
                backing: Backing::WindowsSmb { temp, share },
            })
        }
        #[cfg(not(windows))]
        Err(skip("smb-loopback", "SMB loopback fixture is Windows-only"))
    }
}

impl Drop for FsFixture {
    fn drop(&mut self) {
        match &self.backing {
            Backing::Temp(temp) => {
                let _ = temp.path();
            }
            #[cfg(windows)]
            Backing::WindowsVhd { temp, image } => {
                let script = temp.path().join("detach.txt");
                let body = format!(
                    "select vdisk file=\"{}\"\r\ndetach vdisk\r\n",
                    image.display()
                );
                let _ = std::fs::write(&script, body);
                let _ = Command::new("diskpart")
                    .args(["/s", &script.to_string_lossy()])
                    .output();
            }
            #[cfg(windows)]
            Backing::WindowsSmb { temp, share } => {
                let _ = temp.path();
                let _ = Command::new("net")
                    .args(["share", share, "/delete", "/y"])
                    .output();
            }
            #[cfg(target_os = "linux")]
            Backing::LinuxLoop { temp, device } => {
                let _ = run_privileged("umount", &[&temp.path().join("mount").to_string_lossy()]);
                let _ = run_privileged("losetup", &["-d", device]);
            }
            #[cfg(target_os = "macos")]
            Backing::MacImage { temp, mount } => {
                let _ = temp.path();
                let _ = Command::new("hdiutil")
                    .args(["detach", &mount.to_string_lossy()])
                    .output();
            }
        }
    }
}

fn skip(name: &'static str, reason: impl Into<String>) -> FixtureSkip {
    FixtureSkip {
        name,
        reason: reason.into(),
    }
}

fn command_error(command: &str, output: &Output) -> String {
    format!(
        "{command} failed ({}): {}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[cfg(windows)]
fn windows_vhd(name: &'static str, filesystem: &str) -> FixtureResult {
    windows_vhd_sized(name, filesystem, 1024)
}

#[cfg(windows)]
fn windows_vhd_sized(name: &'static str, filesystem: &str, maximum_mb: u32) -> FixtureResult {
    let temp = tempfile::Builder::new()
        .prefix("zccache-vhdx-")
        .tempdir()
        .map_err(|error| skip(name, error.to_string()))?;
    let image = temp.path().join(format!("{filesystem}.vhdx"));
    let mount = temp.path().join("mount");
    std::fs::create_dir(&mount).map_err(|error| skip(name, error.to_string()))?;
    let script = temp.path().join("create.txt");
    let body = format!(
        "create vdisk file=\"{}\" maximum={maximum_mb} type=expandable\r\nselect vdisk file=\"{}\"\r\nattach vdisk\r\ncreate partition primary\r\nformat fs={} quick label=zccache\r\nassign mount=\"{}\"\r\n",
        image.display(),
        image.display(),
        filesystem,
        mount.display()
    );
    std::fs::write(&script, body).map_err(|error| skip(name, error.to_string()))?;
    let output = Command::new("diskpart")
        .args(["/s", &script.to_string_lossy()])
        .output()
        .map_err(|error| skip(name, format!("diskpart unavailable: {error}")))?;
    if !output.status.success() || std::fs::write(mount.join("probe"), b"probe").is_err() {
        return Err(skip(name, command_error("diskpart", &output)));
    }
    Ok(FsFixture {
        name,
        root: mount,
        backing: Backing::WindowsVhd { temp, image },
    })
}

#[cfg(not(windows))]
fn windows_vhd(name: &'static str, _filesystem: &str) -> FixtureResult {
    Err(skip(name, "VHDX fixtures are Windows-only"))
}

#[cfg(not(windows))]
fn windows_vhd_sized(name: &'static str, _filesystem: &str, _maximum_mb: u32) -> FixtureResult {
    Err(skip(name, "VHDX fixtures are Windows-only"))
}

#[cfg(target_os = "linux")]
fn linux_loop(name: &'static str, filesystem: &str, mkfs: &str) -> FixtureResult {
    for tool in ["losetup", "mount", "umount", mkfs] {
        if Command::new(tool).arg("--help").output().is_err() {
            return Err(skip(name, format!("required tool {tool} is unavailable")));
        }
    }
    let temp = tempfile::Builder::new()
        .prefix("zccache-loop-")
        .tempdir()
        .map_err(|error| skip(name, error.to_string()))?;
    let image = temp.path().join(format!("{filesystem}.img"));
    std::fs::File::create(&image)
        .and_then(|file| file.set_len(512 * 1024 * 1024))
        .map_err(|error| skip(name, error.to_string()))?;
    let loop_output = run_privileged("losetup", &["--find", "--show", &image.to_string_lossy()])
        .map_err(|error| skip(name, error.to_string()))?;
    if !loop_output.status.success() {
        return Err(skip(name, command_error("losetup", &loop_output)));
    }
    let device = String::from_utf8_lossy(&loop_output.stdout)
        .trim()
        .to_string();
    let format_output =
        run_privileged(mkfs, &[&device]).map_err(|error| skip(name, error.to_string()))?;
    if !format_output.status.success() {
        let _ = run_privileged("losetup", &["-d", &device]);
        return Err(skip(name, command_error(mkfs, &format_output)));
    }
    let mount = temp.path().join("mount");
    std::fs::create_dir(&mount).map_err(|error| skip(name, error.to_string()))?;
    let mount_str = mount.to_string_lossy().into_owned();
    // FAT-family filesystems (vfat) have no POSIX permission bits: every
    // file is owned by whoever mounted it, and a later `chmod` on the
    // mountpoint is silently ignored by the kernel driver. Without
    // `uid=/gid=`, a privileged (sudo) mount leaves the tree owned by
    // root and unwritable by the unprivileged CI user. Pass the current
    // uid/gid + a permissive umask explicitly for vfat.
    let uid_gid_opt = (filesystem == "vfat").then(current_uid_gid).flatten();
    let mount_args: Vec<&str> = match &uid_gid_opt {
        Some(opt) => vec!["-t", filesystem, "-o", opt, &device, &mount_str],
        None => vec!["-t", filesystem, &device, &mount_str],
    };
    let mount_output =
        run_privileged("mount", &mount_args).map_err(|error| skip(name, error.to_string()))?;
    if !mount_output.status.success() {
        let _ = run_privileged("losetup", &["-d", &device]);
        return Err(skip(name, command_error("mount", &mount_output)));
    }
    if uid_gid_opt.is_none() {
        let chmod = run_privileged("chmod", &["0777", &mount_str])
            .map_err(|error| skip(name, error.to_string()))?;
        if !chmod.status.success() {
            return Err(skip(name, command_error("chmod", &chmod)));
        }
    }
    Ok(FsFixture {
        name,
        root: mount,
        backing: Backing::LinuxLoop { temp, device },
    })
}

#[cfg(not(target_os = "linux"))]
fn linux_loop(name: &'static str, _filesystem: &str, _mkfs: &str) -> FixtureResult {
    Err(skip(name, "loop fixtures are Linux-only"))
}

#[cfg(target_os = "macos")]
fn mac_image(name: &'static str, filesystem: &str) -> FixtureResult {
    let temp = tempfile::Builder::new()
        .prefix("zccache-hdi-")
        .tempdir()
        .map_err(|error| skip(name, error.to_string()))?;
    let image = temp.path().join(format!("{filesystem}.dmg"));
    let mount = temp.path().join("mount");
    std::fs::create_dir(&mount).map_err(|error| skip(name, error.to_string()))?;
    let create = Command::new("hdiutil")
        .args([
            "create",
            "-size",
            "512m",
            "-fs",
            filesystem,
            "-volname",
            "zccache",
            &image.to_string_lossy(),
        ])
        .output()
        .map_err(|error| skip(name, error.to_string()))?;
    if !create.status.success() {
        return Err(skip(name, command_error("hdiutil create", &create)));
    }
    let attach = Command::new("hdiutil")
        .args([
            "attach",
            "-mountpoint",
            &mount.to_string_lossy(),
            &image.to_string_lossy(),
        ])
        .output()
        .map_err(|error| skip(name, error.to_string()))?;
    if !attach.status.success() {
        return Err(skip(name, command_error("hdiutil attach", &attach)));
    }
    Ok(FsFixture {
        name,
        root: mount.clone(),
        backing: Backing::MacImage { temp, mount },
    })
}

#[cfg(not(target_os = "macos"))]
fn mac_image(name: &'static str, _filesystem: &str) -> FixtureResult {
    Err(skip(name, "disk-image fixtures are macOS-only"))
}

#[cfg(target_os = "linux")]
fn current_uid_gid() -> Option<String> {
    let uid = String::from_utf8(Command::new("id").arg("-u").output().ok()?.stdout)
        .ok()?
        .trim()
        .to_string();
    let gid = String::from_utf8(Command::new("id").arg("-g").output().ok()?.stdout)
        .ok()?
        .trim()
        .to_string();
    Some(format!("uid={uid},gid={gid},umask=000"))
}

#[cfg(target_os = "linux")]
fn run_privileged(program: &str, args: &[&str]) -> std::io::Result<Output> {
    let is_root = Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .is_some_and(|output| output.stdout == b"0\n");
    if is_root {
        Command::new(program).args(args).output()
    } else {
        Command::new("sudo")
            .arg("-n")
            .arg(program)
            .args(args)
            .output()
    }
}

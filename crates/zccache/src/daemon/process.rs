//! Helpers for daemon-owned child processes.

use std::io;
use std::process::Output;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

pub(crate) const COMPILE_PRIORITY_ENV: &str = "ZCCACHE_COMPILE_PRIORITY";
pub const ZCCACHE_COMPILE_PRIORITY_LINK: &str = "ZCCACHE_COMPILE_PRIORITY_LINK";
const AUTO_PRIORITY_SATURATED_CPU_PERCENT: f32 = 95.0;

/// Priority policy for compiler/linker child processes owned by the daemon.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum CompilePriority {
    #[default]
    Auto,
    Normal,
    Low,
    Idle,
    High,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CompilePriorityDecision {
    pub(crate) requested: CompilePriority,
    pub(crate) effective: CompilePriority,
    pub(crate) cpu_usage_percent: Option<f32>,
}

impl CompilePriority {
    pub(crate) fn parse(value: &str) -> Result<Self, CompilePriorityParseError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "normal" => Ok(Self::Normal),
            "low" => Ok(Self::Low),
            "idle" => Ok(Self::Idle),
            "high" => Ok(Self::High),
            other => Err(CompilePriorityParseError {
                value: other.to_string(),
            }),
        }
    }

    pub(crate) fn from_client_env(env: Option<&[(String, String)]>) -> Self {
        let daemon_value = std::env::var(COMPILE_PRIORITY_ENV).ok();
        Self::from_client_env_with_daemon_env(env, daemon_value.as_deref())
    }

    fn from_client_env_with_daemon_env(
        env: Option<&[(String, String)]>,
        daemon_value: Option<&str>,
    ) -> Self {
        if let Some(value) = Self::client_env_value(env, COMPILE_PRIORITY_ENV) {
            return Self::parse_or_warn(value, COMPILE_PRIORITY_ENV);
        }

        match daemon_value {
            Some(value) => Self::parse_or_warn(value, COMPILE_PRIORITY_ENV),
            None => Self::Auto,
        }
    }

    pub(crate) fn from_client_env_for_link_like(
        env: Option<&[(String, String)]>,
        is_link_like: bool,
    ) -> Self {
        let daemon_link_value = std::env::var(ZCCACHE_COMPILE_PRIORITY_LINK).ok();
        let daemon_compile_value = std::env::var(COMPILE_PRIORITY_ENV).ok();
        Self::from_client_env_for_link_like_with_daemon_env(
            env,
            is_link_like,
            daemon_link_value.as_deref(),
            daemon_compile_value.as_deref(),
        )
    }

    fn from_client_env_for_link_like_with_daemon_env(
        env: Option<&[(String, String)]>,
        is_link_like: bool,
        daemon_link_value: Option<&str>,
        daemon_compile_value: Option<&str>,
    ) -> Self {
        if is_link_like {
            if let Some(value) = Self::client_env_value(env, ZCCACHE_COMPILE_PRIORITY_LINK) {
                return Self::parse_or_warn(value, ZCCACHE_COMPILE_PRIORITY_LINK);
            }

            if let Some(value) = daemon_link_value {
                return Self::parse_or_warn(value, ZCCACHE_COMPILE_PRIORITY_LINK);
            }

            return Self::Normal;
        }

        Self::from_client_env_with_daemon_env(env, daemon_compile_value)
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Normal => "normal",
            Self::Low => "low",
            Self::Idle => "idle",
            Self::High => "high",
        }
    }

    pub(crate) fn resolve_for_current_load(self) -> CompilePriorityDecision {
        let cpu_usage_percent = matches!(self, Self::Auto)
            .then(current_cpu_usage_percent)
            .flatten();
        self.resolve_with_cpu_usage(cpu_usage_percent)
    }

    fn resolve_with_cpu_usage(self, cpu_usage_percent: Option<f32>) -> CompilePriorityDecision {
        let effective = match self {
            Self::Auto => Self::auto_effective_priority(cpu_usage_percent),
            priority => priority,
        };
        CompilePriorityDecision {
            requested: self,
            effective,
            cpu_usage_percent,
        }
    }

    fn auto_effective_priority(cpu_usage_percent: Option<f32>) -> Self {
        match cpu_usage_percent {
            Some(cpu) if cpu >= AUTO_PRIORITY_SATURATED_CPU_PERCENT => Self::Low,
            Some(_) | None => Self::Normal,
        }
    }

    fn parse_or_warn(value: &str, env_name: &str) -> Self {
        match Self::parse(value) {
            Ok(priority) => priority,
            Err(e) => {
                tracing::warn!(
                    env = env_name,
                    value = %e.value,
                    "invalid compiler child priority; using low"
                );
                Self::Low
            }
        }
    }

    fn client_env_value<'a>(env: Option<&'a [(String, String)]>, key: &str) -> Option<&'a str> {
        env.and_then(|vars| {
            vars.iter()
                .find(|(candidate, _)| candidate == key)
                .map(|(_, value)| value.as_str())
        })
    }

    #[cfg(test)]
    fn parse_optional(value: Option<&str>) -> Result<Self, CompilePriorityParseError> {
        match value {
            Some(value) => Self::parse(value),
            None => Ok(Self::Auto),
        }
    }

    #[cfg(unix)]
    fn unix_nice_value(self) -> Option<i32> {
        match self {
            Self::Auto | Self::Normal => None,
            Self::Low => Some(10),
            Self::Idle => Some(19),
            // Higher priorities commonly require extra privileges; failures are
            // logged and compilation continues at the inherited priority.
            Self::High => Some(-5),
        }
    }

    #[cfg(windows)]
    fn windows_priority_class(self) -> Option<u32> {
        use windows_sys::Win32::System::Threading::{
            BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS,
        };

        match self {
            Self::Auto | Self::Normal => None,
            Self::Low => Some(BELOW_NORMAL_PRIORITY_CLASS),
            Self::Idle => Some(IDLE_PRIORITY_CLASS),
            Self::High => Some(HIGH_PRIORITY_CLASS),
        }
    }
}

struct CpuUsageMonitor {
    system: sysinfo::System,
    last_refresh: Option<Instant>,
    last_usage_percent: Option<f32>,
}

impl CpuUsageMonitor {
    fn new() -> Self {
        let mut system = sysinfo::System::new();
        system.refresh_cpu_usage();
        Self {
            system,
            last_refresh: Some(Instant::now()),
            last_usage_percent: None,
        }
    }

    fn sample(&mut self) -> Option<f32> {
        let now = Instant::now();
        if self
            .last_refresh
            .is_some_and(|last| now.duration_since(last) < sysinfo::MINIMUM_CPU_UPDATE_INTERVAL)
        {
            return self.last_usage_percent;
        }

        self.system.refresh_cpu_usage();
        self.last_refresh = Some(now);
        let usage = self.system.global_cpu_usage().clamp(0.0, 100.0);
        self.last_usage_percent = Some(usage);
        self.last_usage_percent
    }
}

fn current_cpu_usage_percent() -> Option<f32> {
    static CPU_USAGE_MONITOR: OnceLock<Mutex<CpuUsageMonitor>> = OnceLock::new();
    let monitor = CPU_USAGE_MONITOR.get_or_init(|| Mutex::new(CpuUsageMonitor::new()));
    monitor.lock().ok().and_then(|mut monitor| monitor.sample())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompilePriorityParseError {
    value: String,
}

/// Windows process creation flags applied to daemon-spawned children.
///
/// Currently always returns `CREATE_NO_WINDOW` (`0x08000000`). When the
/// daemon is launched detached (no console attached), spawning a
/// console-subsystem child like rustc / cl / clang without this flag
/// causes Windows to allocate a fresh console window for the child —
/// a visible flash per cache-miss compile in the soldr + rustc +
/// zccache call chain. Setting `CREATE_NO_WINDOW` suppresses that
/// allocation; stdio already flows through the pipes the helpers
/// attach, so no output is lost.
///
/// `priority` is a parameter so future priority bits (`IDLE_PRIORITY_CLASS`,
/// `BELOW_NORMAL_PRIORITY_CLASS`, etc.) can be OR'd in directly at the
/// `CreateProcessW` call rather than via the separate post-spawn
/// `SetPriorityClass` we use today. Unused today — kept for API
/// stability so the call sites don't change shape later.
#[cfg(windows)]
fn child_creation_flags(_priority: CompilePriority) -> u32 {
    /// `CREATE_NO_WINDOW` from `windows_sys::Win32::System::Threading`.
    /// Hardcoded here because the daemon doesn't otherwise pull in
    /// `windows-sys` and a single u32 constant doesn't justify the dep.
    /// Value verified against the Windows SDK header `winbase.h`.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    CREATE_NO_WINDOW
}

/// Wait for a synchronous command after applying a compiler child priority.
///
/// Convenience wrapper that pipes `Stdio::null()` for stdin. Callers that
/// need to forward bytes from the client's stdin (e.g. `rustc -`) use
/// [`command_output_with_priority_stdin`] instead.
pub(crate) fn command_output_with_priority(
    cmd: &mut std::process::Command,
    priority: CompilePriority,
) -> io::Result<Output> {
    command_output_with_priority_stdin(cmd, priority, None)
}

/// Sync variant that pipes `stdin_bytes` into the child's stdin when the
/// slice is `Some` and non-empty. `None` or empty = `Stdio::null()` (the
/// previous behaviour). Use this in the non-cacheable / direct-run path
/// where the wrapper might be ferrying client stdin over IPC.
pub(crate) fn command_output_with_priority_stdin(
    cmd: &mut std::process::Command,
    priority: CompilePriority,
    stdin_bytes: Option<&[u8]>,
) -> io::Result<Output> {
    let decision = priority.resolve_for_current_load();
    let priority = decision.effective;
    let pipe_stdin = matches!(stdin_bytes, Some(b) if !b.is_empty());

    #[cfg(windows)]
    {
        use std::io::Write;
        use std::os::windows::process::CommandExt;
        use std::process::Stdio;

        if pipe_stdin {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.creation_flags(child_creation_flags(priority));
        let mut child = cmd.spawn()?;
        assign_child_to_daemon_job(child.as_raw_handle());
        apply_priority_to_child_windows(child.as_raw_handle(), priority);
        if pipe_stdin {
            if let Some(mut stdin) = child.stdin.take() {
                // Best-effort: stdin write failures land in the child's
                // own error path (it reads EOF / partial input). We still
                // wait_with_output so the caller sees the exit code.
                let _ = stdin.write_all(stdin_bytes.unwrap_or(&[]));
                // Drop closes the pipe — signals EOF to the child.
            }
        }
        child.wait_with_output()
    }

    #[cfg(unix)]
    {
        use std::io::Write;
        use std::process::Stdio;

        if pipe_stdin {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        apply_priority_to_child_unix(child.id(), priority);
        if pipe_stdin {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(stdin_bytes.unwrap_or(&[]));
            }
        }
        child.wait_with_output()
    }

    #[cfg(not(any(unix, windows)))]
    {
        if priority != CompilePriority::Normal {
            tracing::debug!(
                ?priority,
                "compiler child priority is unsupported on this platform"
            );
        }
        let _ = stdin_bytes; // No piping on pure-stub platforms.
        cmd.output()
    }
}

/// Wait for an async command after applying a compiler child priority.
///
/// Convenience wrapper that pipes `Stdio::null()` for stdin. Callers that
/// need to forward client stdin use [`tokio_command_output_with_priority_stdin`].
pub(crate) async fn tokio_command_output_with_priority(
    cmd: &mut tokio::process::Command,
    priority: CompilePriority,
) -> io::Result<Output> {
    tokio_command_output_with_priority_stdin(cmd, priority, None).await
}

/// Async variant that pipes `stdin_bytes` into the child's stdin when the
/// slice is `Some` and non-empty. See [`command_output_with_priority_stdin`].
pub(crate) async fn tokio_command_output_with_priority_stdin(
    cmd: &mut tokio::process::Command,
    priority: CompilePriority,
    stdin_bytes: Option<&[u8]>,
) -> io::Result<Output> {
    let decision = priority.resolve_for_current_load();
    let priority = decision.effective;
    let pipe_stdin = matches!(stdin_bytes, Some(b) if !b.is_empty());

    #[cfg(windows)]
    {
        use std::process::Stdio;
        use tokio::io::AsyncWriteExt;

        if pipe_stdin {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.creation_flags(child_creation_flags(priority));
        let mut child = cmd.spawn()?;
        if let Some(handle) = child.raw_handle() {
            assign_child_to_daemon_job(handle);
            apply_priority_to_child_windows(handle, priority);
        }
        if pipe_stdin {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(stdin_bytes.unwrap_or(&[])).await;
                let _ = stdin.shutdown().await;
            }
        }
        child.wait_with_output().await
    }

    #[cfg(unix)]
    {
        use std::process::Stdio;
        use tokio::io::AsyncWriteExt;

        if pipe_stdin {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        if let Some(pid) = child.id() {
            apply_priority_to_child_unix(pid, priority);
        }
        if pipe_stdin {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(stdin_bytes.unwrap_or(&[])).await;
                let _ = stdin.shutdown().await;
            }
        }
        child.wait_with_output().await
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = stdin_bytes;
        cmd.output().await
    }
}

#[cfg(windows)]
fn assign_child_to_daemon_job(raw_handle: std::os::windows::io::RawHandle) {
    let Some(job) = DAEMON_JOB.get_or_init(WindowsJob::new).as_ref() else {
        return;
    };

    if let Err(e) = job.assign(raw_handle) {
        tracing::debug!("failed to assign child process to daemon job: {e}");
    }
}

#[cfg(unix)]
fn apply_priority_to_child_unix(pid: u32, priority: CompilePriority) {
    let Some(nice) = priority.unix_nice_value() else {
        return;
    };

    let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid as libc::id_t, nice) };
    if rc != 0 {
        tracing::debug!(
            ?priority,
            pid,
            nice,
            error = %io::Error::last_os_error(),
            "failed to set compiler child priority"
        );
    }
}

#[cfg(windows)]
fn apply_priority_to_child_windows(
    raw_handle: std::os::windows::io::RawHandle,
    priority: CompilePriority,
) {
    let Some(priority_class) = priority.windows_priority_class() else {
        return;
    };

    use windows_sys::Win32::System::Threading::SetPriorityClass;

    let ok = unsafe { SetPriorityClass(raw_handle.cast::<std::ffi::c_void>(), priority_class) };
    if ok == 0 {
        tracing::debug!(
            ?priority,
            error = %io::Error::last_os_error(),
            "failed to set compiler child priority"
        );
    }
}

#[cfg(windows)]
static DAEMON_JOB: OnceLock<Option<WindowsJob>> = OnceLock::new();

#[cfg(windows)]
struct WindowsJob {
    handle: usize,
}

#[cfg(windows)]
impl WindowsJob {
    fn new() -> Option<Self> {
        use std::mem::size_of;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            tracing::debug!(
                "failed to create daemon job object: {}",
                io::Error::last_os_error()
            );
            return None;
        }

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

        let ok = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            tracing::debug!(
                "failed to configure daemon job object: {}",
                io::Error::last_os_error()
            );
            unsafe {
                CloseHandle(handle);
            }
            return None;
        }

        tracing::debug!("created daemon child-process job object");
        Some(Self {
            handle: handle as usize,
        })
    }

    fn assign(&self, raw_handle: std::os::windows::io::RawHandle) -> io::Result<()> {
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        let ok = unsafe {
            AssignProcessToJobObject(self.handle as HANDLE, raw_handle.cast::<std::ffi::c_void>())
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for WindowsJob {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        unsafe {
            CloseHandle(self.handle as HANDLE);
        }
    }
}

#[cfg(windows)]
unsafe impl Send for WindowsJob {}

#[cfg(windows)]
unsafe impl Sync for WindowsJob {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_compile_priority_values() {
        assert_eq!(
            CompilePriority::parse("auto").unwrap(),
            CompilePriority::Auto
        );
        assert_eq!(
            CompilePriority::parse("normal").unwrap(),
            CompilePriority::Normal
        );
        assert_eq!(CompilePriority::parse("LOW").unwrap(), CompilePriority::Low);
        assert_eq!(
            CompilePriority::parse(" idle ").unwrap(),
            CompilePriority::Idle
        );
        assert_eq!(
            CompilePriority::parse("high").unwrap(),
            CompilePriority::High
        );
        assert!(CompilePriority::parse("fast").is_err());
    }

    #[test]
    fn formats_compile_priority_for_profiles() {
        assert_eq!(CompilePriority::Auto.as_str(), "auto");
        assert_eq!(CompilePriority::Normal.as_str(), "normal");
        assert_eq!(CompilePriority::Low.as_str(), "low");
        assert_eq!(CompilePriority::Idle.as_str(), "idle");
        assert_eq!(CompilePriority::High.as_str(), "high");
    }

    #[test]
    fn absent_compile_priority_defaults_to_auto() {
        assert_eq!(
            CompilePriority::parse_optional(None).unwrap(),
            CompilePriority::Auto
        );
    }

    #[test]
    fn auto_priority_uses_normal_until_cpu_is_saturated() {
        assert_eq!(
            CompilePriority::auto_effective_priority(None),
            CompilePriority::Normal
        );
        assert_eq!(
            CompilePriority::auto_effective_priority(Some(94.9)),
            CompilePriority::Normal
        );
        assert_eq!(
            CompilePriority::auto_effective_priority(Some(95.0)),
            CompilePriority::Low
        );
        assert_eq!(
            CompilePriority::auto_effective_priority(Some(100.0)),
            CompilePriority::Low
        );
    }

    #[test]
    fn auto_priority_decision_records_effective_priority() {
        let decision = CompilePriority::Auto.resolve_with_cpu_usage(Some(96.0));
        assert_eq!(decision.requested, CompilePriority::Auto);
        assert_eq!(decision.effective, CompilePriority::Low);
        assert_eq!(decision.cpu_usage_percent, Some(96.0));
    }

    #[test]
    fn auto_priority_can_sample_current_load() {
        let decision = CompilePriority::Auto.resolve_for_current_load();
        assert_eq!(decision.requested, CompilePriority::Auto);
        assert!(matches!(
            decision.effective,
            CompilePriority::Normal | CompilePriority::Low
        ));
        if let Some(cpu_usage_percent) = decision.cpu_usage_percent {
            assert!((0.0..=100.0).contains(&cpu_usage_percent));
        }
    }

    #[test]
    fn client_env_selects_high_mode() {
        let env = vec![(COMPILE_PRIORITY_ENV.to_string(), "high".to_string())];
        assert_eq!(
            CompilePriority::from_client_env(Some(&env)),
            CompilePriority::High
        );
    }

    #[test]
    fn client_env_invalid_value_falls_back_to_low() {
        let env = vec![(COMPILE_PRIORITY_ENV.to_string(), "fast".to_string())];
        assert_eq!(
            CompilePriority::from_client_env(Some(&env)),
            CompilePriority::Low
        );
    }

    #[test]
    fn link_priority_env_overrides_link_like_compile_priority() {
        let env = vec![
            (COMPILE_PRIORITY_ENV.to_string(), "low".to_string()),
            (
                ZCCACHE_COMPILE_PRIORITY_LINK.to_string(),
                "high".to_string(),
            ),
        ];

        assert_eq!(
            CompilePriority::from_client_env_for_link_like_with_daemon_env(
                Some(&env),
                true,
                None,
                None
            ),
            CompilePriority::High
        );
    }

    #[test]
    fn daemon_link_priority_env_overrides_link_like_compile_priority() {
        let env = vec![(COMPILE_PRIORITY_ENV.to_string(), "low".to_string())];

        assert_eq!(
            CompilePriority::from_client_env_for_link_like_with_daemon_env(
                Some(&env),
                true,
                Some("high"),
                None
            ),
            CompilePriority::High
        );
    }

    #[test]
    fn link_like_compile_priority_defaults_to_normal_without_link_override() {
        let env = vec![(COMPILE_PRIORITY_ENV.to_string(), "idle".to_string())];

        assert_eq!(
            CompilePriority::from_client_env_for_link_like_with_daemon_env(
                Some(&env),
                true,
                None,
                None
            ),
            CompilePriority::Normal
        );
    }

    #[test]
    fn non_link_compile_priority_preserves_existing_auto_behavior() {
        let env = vec![
            (
                ZCCACHE_COMPILE_PRIORITY_LINK.to_string(),
                "high".to_string(),
            ),
            (COMPILE_PRIORITY_ENV.to_string(), "auto".to_string()),
        ];

        assert_eq!(
            CompilePriority::from_client_env_for_link_like_with_daemon_env(
                Some(&env),
                false,
                Some("idle"),
                None
            ),
            CompilePriority::Auto
        );
    }

    #[test]
    fn invalid_link_priority_env_falls_back_to_low() {
        let env = vec![(
            ZCCACHE_COMPILE_PRIORITY_LINK.to_string(),
            "fast".to_string(),
        )];

        assert_eq!(
            CompilePriority::from_client_env_for_link_like_with_daemon_env(
                Some(&env),
                true,
                None,
                None
            ),
            CompilePriority::Low
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_priority_mapping_is_explicit() {
        assert_eq!(CompilePriority::Auto.unix_nice_value(), None);
        assert_eq!(CompilePriority::Normal.unix_nice_value(), None);
        assert_eq!(CompilePriority::Low.unix_nice_value(), Some(10));
        assert_eq!(CompilePriority::Idle.unix_nice_value(), Some(19));
        assert_eq!(CompilePriority::High.unix_nice_value(), Some(-5));
    }

    #[cfg(windows)]
    #[test]
    fn windows_priority_mapping_is_explicit() {
        use windows_sys::Win32::System::Threading::{
            BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS,
        };

        assert_eq!(CompilePriority::Auto.windows_priority_class(), None);
        assert_eq!(CompilePriority::Normal.windows_priority_class(), None);
        assert_eq!(
            CompilePriority::Low.windows_priority_class(),
            Some(BELOW_NORMAL_PRIORITY_CLASS)
        );
        assert_eq!(
            CompilePriority::Idle.windows_priority_class(),
            Some(IDLE_PRIORITY_CLASS)
        );
        assert_eq!(
            CompilePriority::High.windows_priority_class(),
            Some(HIGH_PRIORITY_CLASS)
        );
    }

    // ── Console-window suppression (Windows only) ───────────────────────
    //
    // When the daemon is launched detached (no console attached) and then
    // spawns a console-subsystem child like rustc / cl / clang via
    // `command_output_with_priority` or `tokio_command_output_with_priority`,
    // Windows allocates a fresh console window for the child *unless* the
    // creation flags include `CREATE_NO_WINDOW`. The console window flashes
    // for the lifetime of the child — visible whenever cargo hits a cache
    // miss and the daemon executes the compiler inline. Reported by the
    // soldr + rustc + zccache workflow.
    //
    // The end-to-end behavior (child having no console window) is hard to
    // test inside `cargo test` because the test runner's own stdio
    // capture makes the test binary console-less, so a child spawned
    // without `CREATE_NO_WINDOW` reads as console-less too — false green.
    // Instead we unit-test the helper that *computes* the creation flags
    // the spawn site applies. If that helper returns the right bits,
    // `cmd.creation_flags(...)` puts them on the CreateProcessW call.

    /// `child_creation_flags` must include `CREATE_NO_WINDOW` (`0x08000000`)
    /// regardless of priority. Without that bit set, a detached daemon's
    /// `command_output_with_priority` spawn allocates a console window per
    /// child (the soldr + rustc cache-miss flash).
    #[cfg(windows)]
    #[test]
    fn child_creation_flags_includes_create_no_window() {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        for priority in [
            CompilePriority::Normal,
            CompilePriority::Low,
            CompilePriority::Idle,
            CompilePriority::High,
        ] {
            let flags = child_creation_flags(priority);
            assert_eq!(
                flags & CREATE_NO_WINDOW,
                CREATE_NO_WINDOW,
                "child_creation_flags({priority:?}) = 0x{flags:08x} must set CREATE_NO_WINDOW (0x08000000) \
                 to suppress the per-child console flash a detached daemon would otherwise produce"
            );
        }
    }
}

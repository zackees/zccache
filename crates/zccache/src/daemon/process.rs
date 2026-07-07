//! Helpers for daemon-owned child processes.

use std::io;
use std::process::Output;
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    Arc, OnceLock,
};

use arc_swap::ArcSwap;

#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

pub(crate) const COMPILE_PRIORITY_ENV: &str = "ZCCACHE_COMPILE_PRIORITY";
pub const ZCCACHE_COMPILE_PRIORITY_LINK: &str = "ZCCACHE_COMPILE_PRIORITY_LINK";
const AUTO_PRIORITY_SATURATED_CPU_PERCENT: f32 = 95.0;

/// Env vars that, when set to a truthy value, indicate the daemon is
/// running on a CI runner rather than an interactive developer host.
/// First match wins. Documented in the issue #813 epic.
const CI_DETECT_ENV_VARS: &[&str] = &[
    "GITHUB_ACTIONS",
    "CI",
    "BUILDKITE",
    "CIRCLECI",
    "GITLAB_CI",
    "TF_BUILD",
    "TEAMCITY_VERSION",
    "JENKINS_URL",
];

/// True when the daemon appears to be running on a CI runner. Inspects
/// the standard env-var set [`CI_DETECT_ENV_VARS`]. The check is cheap
/// (a small number of `getenv` calls), safe to call per-resolution.
///
/// Returns the name of the detected env var as the second tuple element
/// when CI is detected, so startup logs can surface the source.
pub(crate) fn is_ci_host() -> Option<&'static str> {
    is_ci_host_with_env(|name| std::env::var(name).ok())
}

/// Testable variant of [`is_ci_host`] that takes an env lookup closure
/// so tests do not need to mutate the global process env.
pub(crate) fn is_ci_host_with_env<F>(lookup: F) -> Option<&'static str>
where
    F: Fn(&str) -> Option<String>,
{
    for &var in CI_DETECT_ENV_VARS {
        if let Some(value) = lookup(var) {
            if is_truthy(&value) {
                return Some(var);
            }
        }
    }
    None
}

fn is_truthy(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }
    !matches!(
        trimmed.to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off" | "n"
    )
}

/// Run a CPU/IO-heavy **synchronous** section without stalling the async
/// runtime (issue #955 — daemon-side root cause).
///
/// The miss-store tail of a full-codegen compile does two size-scaling
/// synchronous things on the tokio worker thread: a rayon parallel hash of
/// the source + the whole extern set, and the artifact persist (a large
/// `.rlib` copy when it can't be hardlinked cross-volume). For the
/// consolidated `zccache` crate the extern set is the entire workspace, so
/// each miss parks a worker for a long time. Under several concurrent
/// `cargo test` invocations this can park *every* worker at once — the
/// runtime then can't drive the reply I/O for any in-flight compile, so
/// the daemon "never responds" (0 rustc alive, since rustc already exited)
/// and the client wedges. That is the #955 wedge.
///
/// On the multi-thread daemon runtime, [`tokio::task::block_in_place`] tells
/// tokio to spin up a replacement worker for the duration of `f`, so the
/// runtime keeps servicing other compiles' I/O (including sending their
/// replies) while this section runs. On a current-thread runtime (the
/// embedded host path) `block_in_place` would panic, so `f` runs inline —
/// the pre-#955 status quo, no worse than before.
pub(crate) fn run_cpu_blocking<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    let is_multi_thread = matches!(
        tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()),
        Ok(tokio::runtime::RuntimeFlavor::MultiThread)
    );
    if is_multi_thread {
        tokio::task::block_in_place(f)
    } else {
        f()
    }
}

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
        Self::from_client_env_for_link_like_with_daemon_env_ci(
            env,
            is_link_like,
            daemon_link_value,
            daemon_compile_value,
            is_ci_host().is_some(),
        )
    }

    fn from_client_env_for_link_like_with_daemon_env_ci(
        env: Option<&[(String, String)]>,
        is_link_like: bool,
        daemon_link_value: Option<&str>,
        daemon_compile_value: Option<&str>,
        is_ci: bool,
    ) -> Self {
        if is_link_like {
            if let Some(value) = Self::client_env_value(env, ZCCACHE_COMPILE_PRIORITY_LINK) {
                return Self::parse_or_warn(value, ZCCACHE_COMPILE_PRIORITY_LINK);
            }

            if let Some(value) = daemon_link_value {
                return Self::parse_or_warn(value, ZCCACHE_COMPILE_PRIORITY_LINK);
            }

            // Issue #813 / #810: linker priority is the single biggest UI
            // win on Windows (link.exe is the worst single-thread hog).
            // Interactive hosts default to `Low`; CI keeps the historical
            // `Normal` so dedicated runners don't yield.
            return if is_ci { Self::Normal } else { Self::Low };
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

    /// Observation variant — samples the in-flight counter without
    /// incrementing it. Spawn sites must call [`Self::resolve_and_track`]
    /// instead so the decision is race-free against concurrent spawns.
    pub(crate) fn resolve_for_current_load(self) -> CompilePriorityDecision {
        let cpu_usage_percent = matches!(self, Self::Auto)
            .then(current_cpu_usage_percent)
            .flatten();
        let in_flight = current_in_flight_compiles().saturating_add(current_host_in_flight());
        self.resolve_with_cpu_usage_and_ci(cpu_usage_percent, is_ci_host().is_some(), in_flight)
    }

    /// Spawn-site resolution. Acquires an [`InFlightCompileTicket`] so the
    /// `Auto` decision uses the pre-increment count (race-free under
    /// parallel spawns) and the counter accurately reflects in-flight work
    /// for the next caller. The ticket **must** be held by the caller for
    /// the lifetime of the spawned process.
    ///
    /// When an embedded host has registered an in-flight counter via
    /// [`crate::embedded::ServiceLimits::host_in_flight`] (zccache#924),
    /// its current value is added to the pre-increment count before the
    /// decision is made. This keeps wave-priority semantics correct
    /// across both products' subprocess pressure on the same machine —
    /// otherwise zccache would see `in_flight = 0` and pick `Normal`
    /// even when the host already has dozens of its own rustc children
    /// hammering the CPU.
    pub(crate) fn resolve_and_track(self) -> (CompilePriorityDecision, InFlightCompileTicket) {
        let ticket = InFlightCompileTicket::acquire();
        let cpu_usage_percent = matches!(self, Self::Auto)
            .then(current_cpu_usage_percent)
            .flatten();
        let in_flight = ticket
            .in_flight_before()
            .saturating_add(current_host_in_flight());
        let decision = self.resolve_with_cpu_usage_and_ci(
            cpu_usage_percent,
            is_ci_host().is_some(),
            in_flight,
        );
        (decision, ticket)
    }

    fn resolve_with_cpu_usage_and_ci(
        self,
        cpu_usage_percent: Option<f32>,
        is_ci: bool,
        in_flight_before: usize,
    ) -> CompilePriorityDecision {
        let effective = match self {
            Self::Auto => Self::auto_effective_priority(cpu_usage_percent, is_ci, in_flight_before),
            priority => priority,
        };
        CompilePriorityDecision {
            requested: self,
            effective,
            cpu_usage_percent,
        }
    }

    /// Resolves what `Auto` actually means for a given CPU sample + host
    /// kind + in-flight count. Issue #813 / #810 changed the interactive
    /// default from `Normal` to `Low`; this refinement (master-profile
    /// 2026-06-25 ISSUE-001) restores `Normal` for the case the original
    /// patch overshot — single/idle compiles — while keeping the wave
    /// case at `Low`.
    ///
    /// - **CI host** (any env in [`CI_DETECT_ENV_VARS`] truthy): the
    ///   historical behavior — `Normal` until system CPU is ≥ 95%, then
    ///   `Low`. CI runners are dedicated to compilation; no foreground
    ///   workload to yield to.
    /// - **Interactive host** (no CI env): `Normal` when no other compile
    ///   is in flight (`in_flight_before == 0`), `Low` otherwise. A
    ///   parallel cargo wave of N rustcs all calling `fetch_add(1)` sees
    ///   counts `0, 1, …, N-1` deterministically — one `Normal` leader
    ///   plus `N-1` `Low` followers. The leader at `Normal` runs at
    ///   bare-rustc speed (the cold-bench win); the `N-1` followers stay
    ///   `Low`, bounding the CPU spike #813 was protecting against. When
    ///   the wave finishes and a single rustc returns (e.g. last compile
    ///   in a cargo dep tree, or a one-off check), the counter drops back
    ///   to 0 and the next spawn is again `Normal`.
    fn auto_effective_priority(
        cpu_usage_percent: Option<f32>,
        is_ci: bool,
        in_flight_before: usize,
    ) -> Self {
        if !is_ci {
            if in_flight_before == 0 {
                return Self::Normal;
            }
            return Self::Low;
        }
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

const CPU_USAGE_UNKNOWN_BITS: u32 = u32::MAX;

struct CpuUsageMonitor {
    sampler_started: AtomicBool,
    last_usage_percent_bits: AtomicU32,
}

impl Default for CpuUsageMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuUsageMonitor {
    fn new() -> Self {
        Self {
            sampler_started: AtomicBool::new(false),
            last_usage_percent_bits: AtomicU32::new(CPU_USAGE_UNKNOWN_BITS),
        }
    }

    fn sample(&'static self) -> Option<f32> {
        self.ensure_sampler_started();
        match self.last_usage_percent_bits.load(Ordering::Relaxed) {
            CPU_USAGE_UNKNOWN_BITS => None,
            usage_bits => Some(f32::from_bits(usage_bits)),
        }
    }

    fn ensure_sampler_started(&'static self) {
        if self
            .sampler_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        if let Err(error) = std::thread::Builder::new()
            .name("zccache-cpu-usage-sampler".to_string())
            .spawn(move || self.run_sampler())
        {
            self.sampler_started.store(false, Ordering::Release);
            tracing::debug!(%error, "failed to start CPU usage sampler");
        }
    }

    fn run_sampler(&'static self) {
        let mut system = sysinfo::System::new();
        system.refresh_cpu_usage();

        loop {
            std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
            system.refresh_cpu_usage();
            let usage = average_cpu_usage_percent(&system).clamp(0.0, 100.0);
            self.last_usage_percent_bits
                .store(usage.to_bits(), Ordering::Relaxed);
        }
    }
}

fn average_cpu_usage_percent(system: &sysinfo::System) -> f32 {
    let cpus = system.cpus();
    if cpus.is_empty() {
        return 0.0;
    }
    cpus.iter().map(|cpu| cpu.cpu_usage()).sum::<f32>() / cpus.len() as f32
}

fn current_cpu_usage_percent() -> Option<f32> {
    static CPU_USAGE_MONITOR: OnceLock<CpuUsageMonitor> = OnceLock::new();
    CPU_USAGE_MONITOR.get_or_init(CpuUsageMonitor::new).sample()
}

/// Global counter of daemon-owned compiler children currently spawned and
/// not yet reaped. Used by `Auto` priority to decide whether a new spawn
/// is the first/only one (use `Normal`, restoring near-bare-rustc speed)
/// or part of an in-flight wave (demote to `Low` to preserve UI
/// responsiveness per issues #813 / #810).
static IN_FLIGHT_COMPILES: AtomicUsize = AtomicUsize::new(0);

/// Optional host-supplied in-flight counter (zccache#924). When an
/// embedded `ZccacheService` runs inside a larger host daemon (soldr,
/// fbuild), that host may spawn its own subprocess children that
/// zccache's local counter never sees. If the host clones a shared
/// counter into [`crate::embedded::ServiceLimits::host_in_flight`],
/// `ZccacheService::start` registers it here and
/// [`CompilePriority::auto_effective_priority`] sums its current value
/// into the in-flight count used to decide `Normal` vs `Low`.
///
/// Stored as `ArcSwap<Option<Arc<AtomicUsize>>>` so reads on the hot
/// path are wait-free and the registration / deregistration cost is
/// only paid at service start / shutdown. The contract is single-slot:
/// only one embedded service per process can have its counter
/// registered at a time, matching the canonical "one host + one
/// embedded zccache" deployment. A second registration overwrites the
/// first (with a `tracing::warn!` so the double-register case is
/// debuggable).
static HOST_IN_FLIGHT: OnceLock<ArcSwap<Option<Arc<AtomicUsize>>>> = OnceLock::new();

fn host_in_flight_slot() -> &'static ArcSwap<Option<Arc<AtomicUsize>>> {
    HOST_IN_FLIGHT.get_or_init(|| ArcSwap::from_pointee(None))
}

/// Read the current host-side in-flight count, or 0 if no host counter
/// has been registered. Hot path — wait-free read of the `ArcSwap`
/// guard then an `Acquire` load on the inner atomic.
fn current_host_in_flight() -> usize {
    host_in_flight_slot()
        .load()
        .as_ref()
        .as_ref()
        .map(|counter| counter.load(Ordering::Acquire))
        .unwrap_or(0)
}

/// Register a host-supplied in-flight counter (zccache#924). Called
/// from [`crate::embedded::ZccacheService::start`] when the caller
/// populated [`crate::embedded::ServiceLimits::host_in_flight`].
///
/// Returns an RAII guard that clears the slot on drop, so a host that
/// drops its `ZccacheService` automatically deregisters its counter and
/// the priority decision falls back to the zccache-internal counter
/// only. Multiple registrations from the same process race; the latest
/// wins and a warning is logged.
pub(crate) fn register_host_in_flight_counter(counter: Arc<AtomicUsize>) -> HostInFlightGuard {
    let slot = host_in_flight_slot();
    let previous = slot.swap(Arc::new(Some(counter)));
    if previous.is_some() {
        tracing::warn!(
            "host in-flight counter already registered; overwriting (zccache#924). \
             Only one embedded ZccacheService should run per process."
        );
    }
    HostInFlightGuard { _marker: () }
}

/// RAII guard returned by [`register_host_in_flight_counter`]. Drop
/// clears the slot, restoring the zccache-internal-only Auto priority
/// behavior.
#[must_use = "the guard clears the host counter slot on drop — hold it for the service lifetime"]
pub(crate) struct HostInFlightGuard {
    _marker: (),
}

impl Drop for HostInFlightGuard {
    fn drop(&mut self) {
        host_in_flight_slot().store(Arc::new(None));
    }
}

/// Observation-only sampler. `Auto` resolution at *spawn sites* must use
/// the pre-increment value returned by [`InFlightCompileTicket::acquire`]
/// instead, so concurrent decisions are race-free (fetch-add ordering).
pub(crate) fn current_in_flight_compiles() -> usize {
    IN_FLIGHT_COMPILES.load(Ordering::Acquire)
}

/// RAII ticket representing one in-flight compile spawn. Acquire **before**
/// resolving `Auto` priority; the pre-increment count is exposed via
/// [`Self::in_flight_before`] and is the deterministic input for the
/// priority decision (a wave of N simultaneous fetch-adds yields counts
/// `0, 1, 2, …, N-1` — one `Normal` followed by `N-1` `Low`, bounding the
/// CPU spike #813 was protecting against).
///
/// Drop decrements the counter; the ticket must be held until the spawned
/// process is fully waited on.
#[must_use = "the ticket decrements on drop — hold it until the child is reaped"]
pub(crate) struct InFlightCompileTicket {
    in_flight_before: usize,
}

impl InFlightCompileTicket {
    pub(crate) fn acquire() -> Self {
        let in_flight_before = IN_FLIGHT_COMPILES.fetch_add(1, Ordering::AcqRel);
        Self { in_flight_before }
    }

    pub(crate) fn in_flight_before(&self) -> usize {
        self.in_flight_before
    }
}

impl Drop for InFlightCompileTicket {
    fn drop(&mut self) {
        IN_FLIGHT_COMPILES.fetch_sub(1, Ordering::AcqRel);
    }
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

/// Apply the daemon's console-suppression creation flag (`CREATE_NO_WINDOW`)
/// to a child spawned OUTSIDE the priority-aware `command_output_*` helpers.
///
/// The compiler-identity probe (`rustc -vV` in [`super::server`]'s
/// `compiler_hash`) builds a `Command` and calls `.output()` directly, so it
/// never reached [`child_creation_flags`]. Since the daemon runs detached
/// (no console), that made every cold-path identity probe flash a console
/// window on Windows — the exact symptom `child_creation_flags` was added to
/// fix for the compile path. This routes those probes through the same flag.
/// No-op on non-Windows.
pub(crate) fn suppress_child_console(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(child_creation_flags(CompilePriority::Normal));
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

/// `tokio::process::Command` variant of [`suppress_child_console`].
///
/// `tokio::process::Command::creation_flags` is an inherent method (unlike
/// `std`'s, which comes from `CommandExt`), so no trait import is needed.
pub(crate) fn suppress_child_console_tokio(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    cmd.creation_flags(child_creation_flags(CompilePriority::Normal));
    #[cfg(not(windows))]
    let _ = cmd;
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
    let (decision, _ticket) = priority.resolve_and_track();
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

/// Wait for an async command after applying compiler child priority, killing
/// the child and returning `TimedOut` when `timeout` elapses.
pub(crate) async fn tokio_command_output_with_priority_timeout(
    cmd: &mut tokio::process::Command,
    priority: CompilePriority,
    timeout: std::time::Duration,
) -> io::Result<Output> {
    let wait = tokio_command_output_with_priority_stdin(cmd, priority, None);
    match tokio::time::timeout(timeout, wait).await {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("child process timed out after {timeout:?}"),
        )),
    }
}

/// Async variant that pipes `stdin_bytes` into the child's stdin when the
/// slice is `Some` and non-empty. See [`command_output_with_priority_stdin`].
pub(crate) async fn tokio_command_output_with_priority_stdin(
    cmd: &mut tokio::process::Command,
    priority: CompilePriority,
    stdin_bytes: Option<&[u8]>,
) -> io::Result<Output> {
    let (decision, _ticket) = priority.resolve_and_track();
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
        cmd.kill_on_drop(true);
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
        cmd.kill_on_drop(true);
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

    // ── run_cpu_blocking (#955) ──

    #[test]
    fn run_cpu_blocking_no_runtime_runs_inline() {
        // Outside any tokio runtime the section runs inline and returns
        // the closure's value.
        assert_eq!(run_cpu_blocking(|| 40 + 2), 42);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_cpu_blocking_multi_thread_ok() {
        // On the daemon's real (multi-thread) runtime this takes the
        // block_in_place branch and must still return the value.
        assert_eq!(run_cpu_blocking(|| "ok"), "ok");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_cpu_blocking_current_thread_does_not_panic() {
        // Regression guard: block_in_place panics on a current-thread
        // runtime (the embedded-host path), so run_cpu_blocking MUST fall
        // back to running inline there rather than aborting the compile.
        assert_eq!(run_cpu_blocking(|| 123), 123);
    }

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
    fn ci_auto_priority_uses_normal_until_cpu_is_saturated() {
        // CI host (is_ci=true) preserves the historical heuristic:
        // Normal until 95% CPU, then Low. CI runners are dedicated to
        // compilation; no foreground workload to yield to. In-flight
        // count is ignored on CI — the CPU gate is sufficient.
        let is_ci = true;
        assert_eq!(
            CompilePriority::auto_effective_priority(None, is_ci, 0),
            CompilePriority::Normal
        );
        assert_eq!(
            CompilePriority::auto_effective_priority(Some(94.9), is_ci, 32),
            CompilePriority::Normal
        );
        assert_eq!(
            CompilePriority::auto_effective_priority(Some(95.0), is_ci, 0),
            CompilePriority::Low
        );
        assert_eq!(
            CompilePriority::auto_effective_priority(Some(100.0), is_ci, 32),
            CompilePriority::Low
        );
    }

    #[test]
    fn interactive_auto_priority_adapts_to_in_flight_count() {
        // Master-profile 2026-06-25 ISSUE-001: interactive hosts get
        // Normal when no other compile is in flight (single/idle case —
        // bare-rustc speed), Low once a wave is detected. Preserves
        // #813's UI-win on parallel waves while restoring near-bare-rustc
        // speed on the single-compile cases that the unconditional Low
        // was overshooting.
        let is_ci = false;
        // No others in flight → Normal regardless of CPU.
        assert_eq!(
            CompilePriority::auto_effective_priority(None, is_ci, 0),
            CompilePriority::Normal
        );
        assert_eq!(
            CompilePriority::auto_effective_priority(Some(0.0), is_ci, 0),
            CompilePriority::Normal
        );
        assert_eq!(
            CompilePriority::auto_effective_priority(Some(100.0), is_ci, 0),
            CompilePriority::Normal
        );
        // One or more others in flight → Low (yield to UI).
        assert_eq!(
            CompilePriority::auto_effective_priority(None, is_ci, 1),
            CompilePriority::Low
        );
        assert_eq!(
            CompilePriority::auto_effective_priority(Some(50.0), is_ci, 7),
            CompilePriority::Low
        );
    }

    #[test]
    fn auto_priority_decision_records_effective_priority_on_ci() {
        let decision = CompilePriority::Auto.resolve_with_cpu_usage_and_ci(Some(96.0), true, 0);
        assert_eq!(decision.requested, CompilePriority::Auto);
        assert_eq!(decision.effective, CompilePriority::Low);
        assert_eq!(decision.cpu_usage_percent, Some(96.0));
    }

    #[test]
    fn auto_priority_decision_low_on_interactive_when_wave_in_flight() {
        let decision = CompilePriority::Auto.resolve_with_cpu_usage_and_ci(Some(10.0), false, 3);
        assert_eq!(decision.requested, CompilePriority::Auto);
        assert_eq!(decision.effective, CompilePriority::Low);
        assert_eq!(decision.cpu_usage_percent, Some(10.0));
    }

    #[test]
    fn auto_priority_decision_normal_on_interactive_when_idle() {
        let decision = CompilePriority::Auto.resolve_with_cpu_usage_and_ci(Some(10.0), false, 0);
        assert_eq!(decision.requested, CompilePriority::Auto);
        assert_eq!(decision.effective, CompilePriority::Normal);
        assert_eq!(decision.cpu_usage_percent, Some(10.0));
    }

    #[test]
    fn in_flight_ticket_returns_pre_increment_count_atomically() {
        let baseline = current_in_flight_compiles();
        let t1 = InFlightCompileTicket::acquire();
        assert_eq!(t1.in_flight_before(), baseline);
        assert_eq!(current_in_flight_compiles(), baseline + 1);
        let t2 = InFlightCompileTicket::acquire();
        assert_eq!(t2.in_flight_before(), baseline + 1);
        assert_eq!(current_in_flight_compiles(), baseline + 2);
        drop(t2);
        assert_eq!(current_in_flight_compiles(), baseline + 1);
        drop(t1);
        assert_eq!(current_in_flight_compiles(), baseline);
    }

    /// zccache#924: serialize tests that touch the process-wide host
    /// in-flight slot. Without this, parallel test execution sees the
    /// "single-slot, last-write-wins" contract collide between cases.
    static HOST_INFLIGHT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn host_counter_zero_when_unregistered() {
        let _guard = HOST_INFLIGHT_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // No registration: `current_host_in_flight()` returns 0 and
        // auto-priority falls back to today's behavior.
        assert_eq!(current_host_in_flight(), 0);
        let decision = CompilePriority::Auto.resolve_with_cpu_usage_and_ci(Some(10.0), false, 0);
        assert_eq!(decision.effective, CompilePriority::Normal);
    }

    #[test]
    fn host_counter_summed_into_auto_priority_decision() {
        let _serial = HOST_INFLIGHT_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // zccache#924 acceptance criterion: configure a host counter
        // showing 5 in-flight host spawns and assert the read of
        // `current_host_in_flight()` reflects it. Feed that value into
        // `resolve_with_cpu_usage_and_ci(_, is_ci=false, _)` directly so
        // the assertion holds regardless of the test runner — CI
        // detection on GitHub Actions routes Auto through the CI branch
        // that ignores `in_flight_before`, so a test that calls
        // `resolve_for_current_load` would be non-portable.
        let counter = Arc::new(AtomicUsize::new(5));
        let _registration_guard = register_host_in_flight_counter(Arc::clone(&counter));
        assert_eq!(current_host_in_flight(), 5);

        let summed = current_in_flight_compiles().saturating_add(current_host_in_flight());
        assert!(summed >= 5, "host counter must be summed into in-flight");
        let decision =
            CompilePriority::Auto.resolve_with_cpu_usage_and_ci(Some(10.0), false, summed);
        assert_eq!(
            decision.effective,
            CompilePriority::Low,
            "Auto must demote to Low when host counter says the box is busy",
        );

        // Bring the host counter back to 0 and confirm the next read
        // sees the change.
        counter.store(0, Ordering::Release);
        assert_eq!(current_host_in_flight(), 0);
        let summed = current_in_flight_compiles().saturating_add(current_host_in_flight());
        let decision =
            CompilePriority::Auto.resolve_with_cpu_usage_and_ci(Some(10.0), false, summed);
        // With host_in_flight = 0 and no concurrent zccache ticket held,
        // the summed count is 0 and interactive Auto picks Normal.
        assert_eq!(
            decision.effective,
            CompilePriority::Normal,
            "after host counter drops to 0 the interactive Auto decision must be Normal",
        );
    }

    #[test]
    fn host_inflight_guard_clears_slot_on_drop() {
        let _serial = HOST_INFLIGHT_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let counter = Arc::new(AtomicUsize::new(7));
        {
            let _guard = register_host_in_flight_counter(Arc::clone(&counter));
            assert_eq!(current_host_in_flight(), 7);
        }
        // RAII guard dropped — slot must be empty again so subsequent
        // tests / future starts see the clean state.
        assert_eq!(
            current_host_in_flight(),
            0,
            "dropping the host-inflight guard must restore the zccache-internal-only baseline"
        );
    }

    #[test]
    fn host_counter_saturates_without_overflow() {
        let _serial = HOST_INFLIGHT_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Defensive: a pathological host counter near usize::MAX must
        // not overflow when summed with the ticket's pre-increment
        // count. The implementation uses `saturating_add` for exactly
        // this case — guard the contract here so a refactor cannot
        // regress to wrapping arithmetic.
        //
        // Use explicit `is_ci = false` so the assertion holds on both
        // CI runners and interactive hosts.
        let counter = Arc::new(AtomicUsize::new(usize::MAX));
        let _guard = register_host_in_flight_counter(Arc::clone(&counter));
        let summed = 1usize.saturating_add(current_host_in_flight());
        assert_eq!(
            summed,
            usize::MAX,
            "saturating_add must clamp at usize::MAX"
        );
        let decision =
            CompilePriority::Auto.resolve_with_cpu_usage_and_ci(Some(10.0), false, summed);
        assert_eq!(decision.effective, CompilePriority::Low);
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
    fn link_like_compile_priority_on_ci_defaults_to_normal_without_link_override() {
        let env = vec![(COMPILE_PRIORITY_ENV.to_string(), "idle".to_string())];

        assert_eq!(
            CompilePriority::from_client_env_for_link_like_with_daemon_env_ci(
                Some(&env),
                true,
                None,
                None,
                true, // is_ci
            ),
            CompilePriority::Normal
        );
    }

    #[test]
    fn link_like_compile_priority_on_interactive_defaults_to_low_without_link_override() {
        // Issue #813 / #810: link.exe is the single worst single-thread
        // hog on Windows MSVC. Interactive hosts demote it to Low so the
        // late-build link step doesn't lock up the UI.
        let env = vec![(COMPILE_PRIORITY_ENV.to_string(), "idle".to_string())];

        assert_eq!(
            CompilePriority::from_client_env_for_link_like_with_daemon_env_ci(
                Some(&env),
                true,
                None,
                None,
                false, // interactive
            ),
            CompilePriority::Low
        );
    }

    #[test]
    fn is_ci_host_detects_known_env_vars() {
        let make_lookup = |hit: &'static str| {
            move |name: &str| {
                if name == hit {
                    Some("true".to_string())
                } else {
                    None
                }
            }
        };
        for var in CI_DETECT_ENV_VARS {
            let detected = is_ci_host_with_env(make_lookup(var));
            assert_eq!(
                detected,
                Some(*var),
                "is_ci_host_with_env failed to detect {var}",
            );
        }
    }

    #[test]
    fn is_ci_host_treats_falsy_values_as_interactive() {
        for falsy in ["0", "false", "FALSE", "no", "off", "n", "", "   "] {
            let lookup = |_name: &str| Some(falsy.to_string());
            assert_eq!(
                is_ci_host_with_env(lookup),
                None,
                "value {falsy:?} should NOT be treated as CI",
            );
        }
    }

    #[test]
    fn is_ci_host_returns_none_when_no_env_set() {
        let lookup = |_name: &str| None;
        assert_eq!(is_ci_host_with_env(lookup), None);
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

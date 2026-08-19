//! Helpers for daemon-owned child processes.

use std::io;
use std::process::Output;
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    Arc, OnceLock,
};

use arc_swap::ArcSwap;

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
        let in_flight = total_in_flight(current_in_flight_compiles());
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
        let in_flight = total_in_flight(ticket.in_flight_before());
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

    fn platform_priority(self) -> crate::platform::process::priority::Priority {
        use crate::platform::process::priority::Priority;

        match self {
            Self::Auto | Self::Normal => Priority::Normal,
            Self::Low => Priority::Low,
            Self::Idle => Priority::Idle,
            Self::High => Priority::High,
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

fn total_in_flight(zccache_in_flight: usize) -> usize {
    zccache_in_flight.saturating_add(current_host_in_flight())
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

/// Child creation is delegated to the running-process boundary.
///
/// This keeps Windows console policy out of zccache while preserving Tokio's
/// async pipe and wait interfaces. In particular, it prevents a visible flash
/// per cache-miss compile in the soldr + rustc + zccache call chain.
///
/// The compiler-identity probe (`rustc -vV` in [`super::server`]'s
/// compiler-hash module) is covered by the same boundary.
///
/// Priority remains local post-spawn policy. Owner-death containment is
/// configured transactionally by running-process on Linux and macOS; Windows
/// retains zccache's established process-wide kill-on-close Job Object until
/// the dependency's Windows spawn path can propagate assignment failures.
///
/// Wait for an async command after applying a compiler child priority.
///
/// Convenience wrapper that pipes `Stdio::null()` for stdin. Callers that
/// need to forward client stdin use [`tokio_command_output_with_priority_stdin`].
/// Spawn options for daemon-owned compiler/tool children (soldr#2442 slice 3):
/// reap the child when the daemon drops the compile future (a client
/// disconnect / Ctrl-C) and when the daemon process itself dies, so a cancelled
/// or crashed compile never orphans a rustc process. running-process provides
/// both primitives: `kill_on_drop` handles a dropped Tokio future, while the
/// platform owner-death policy binds the child to the daemon process.
///
/// Linux and macOS install that containment transactionally in the dependency
/// spawn boundary. Windows keeps the existing local kill-on-close Job Object,
/// so it must not request the dependency's second Job Object as well.
fn owned_child_spawn_options() -> running_process::TokioSpawnOptions {
    running_process::TokioSpawnOptions {
        kill_on_drop: true,
        kill_when_owner_dies: crate::platform::process::spawn::uses_pre_spawn_owner_death(),
        ..Default::default()
    }
}

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
/// slice is `Some` and non-empty. `None` or empty pipes `Stdio::null()`.
///
/// The wait flows through the orphan-pipe watchdog in
/// `crate::daemon::child_watchdog` so a child that leaves a pipe-holding
/// grandchild cannot park the daemon forever (issue #962).
pub(crate) async fn tokio_command_output_with_priority_stdin(
    cmd: &mut tokio::process::Command,
    priority: CompilePriority,
    stdin_bytes: Option<&[u8]>,
) -> io::Result<Output> {
    tokio_command_output_with_priority_stdin_inner(cmd, priority, stdin_bytes, None).await
}

/// Streaming counterpart used by the embedded compile API. Process setup,
/// priority, job assignment, stdin, and watchdog behavior are identical to
/// [`tokio_command_output_with_priority_stdin`]; only pipe capture differs.
pub(crate) async fn tokio_command_output_streaming_with_priority_stdin(
    cmd: &mut tokio::process::Command,
    priority: CompilePriority,
    stdin_bytes: Option<&[u8]>,
    sender: tokio::sync::mpsc::Sender<crate::daemon::compile_output::RawOutputChunk>,
) -> io::Result<Output> {
    tokio_command_output_with_priority_stdin_inner(cmd, priority, stdin_bytes, Some(sender)).await
}

async fn tokio_command_output_with_priority_stdin_inner(
    cmd: &mut tokio::process::Command,
    priority: CompilePriority,
    stdin_bytes: Option<&[u8]>,
    stream: Option<tokio::sync::mpsc::Sender<crate::daemon::compile_output::RawOutputChunk>>,
) -> io::Result<Output> {
    let (decision, _ticket) = priority.resolve_and_track();
    let priority = decision.effective;
    let pipe_stdin = matches!(stdin_bytes, Some(b) if !b.is_empty());
    // Human-readable program id for the child-wait watchdog diagnostics
    // (issue #962). Captured before the command is mutated/spawned.
    let cmd_desc = cmd.as_std().get_program().to_string_lossy().into_owned();

    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;

    if pipe_stdin {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = running_process::spawn_tokio(cmd, owned_child_spawn_options())?;
    attach_child_owner_death(&child);
    apply_priority_to_child(&child, priority);
    if pipe_stdin {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(stdin_bytes.unwrap_or(&[])).await;
            let _ = stdin.shutdown().await;
        }
    }
    match stream {
        Some(sender) => {
            crate::daemon::child_watchdog::wait_with_output_watchdog_streaming(
                child, &cmd_desc, sender,
            )
            .await
        }
        None => crate::daemon::child_watchdog::wait_with_output_watchdog(child, &cmd_desc).await,
    }
}

/// Run a leaf tool without the orphan-pipe watchdog used for compilers.
/// Pure archivers do not spawn descendants, so Tokio can reap them directly.
pub(crate) async fn tokio_leaf_command_output_with_priority(
    cmd: &mut tokio::process::Command,
    priority: CompilePriority,
) -> io::Result<Output> {
    use std::process::Stdio;

    let (decision, _ticket) = priority.resolve_and_track();
    let priority = decision.effective;
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let child = running_process::spawn_tokio(cmd, owned_child_spawn_options())?;
    attach_child_owner_death(&child);
    apply_priority_to_child(&child, priority);
    child.wait_with_output().await
}

fn attach_child_owner_death(child: &tokio::process::Child) {
    if let Err(error) = crate::platform::process::spawn::attach_owner_death(child) {
        tracing::debug!(%error, "failed to attach child process owner-death primitive");
    }
}

fn apply_priority_to_child(child: &tokio::process::Child, priority: CompilePriority) {
    if let Err(error) =
        crate::platform::process::priority::apply_to_child(child, priority.platform_priority())
    {
        tracing::debug!(
            ?priority,
            %error,
            "failed to set compiler child priority"
        );
    }
}

#[cfg(test)]
#[path = "process_tests.rs"]
mod tests;
// test inside `cargo test` because the test runner's own stdio

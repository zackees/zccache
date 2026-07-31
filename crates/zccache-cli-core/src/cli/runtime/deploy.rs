//! Daemon-binary deployment and spawn (issue #1007's self-deploy model).
//!
//! Split out of `runtime.rs` when it crossed the 1.5K-LOC guard. This half
//! owns everything between "decide to start a daemon" and "a process is
//! running": where the binary lives, verifying it before it is executed
//! (#1172), the spawn-log directory, and the spawn itself. The recovery
//! ladder that decides *whether* to spawn stays in `mod.rs`.

use super::*;

/// Initialize spawn-lineage env vars on a command the CLI is about to spawn.
///
/// Mirrors the daemon-side propagation in `zccache_daemon::lineage` so that
/// any process attribution (orphan tracking, running-process scanners) sees
/// a consistent chain across CLI -> daemon -> compiler hops. The chain is
/// initialized with the CLI's PID, and the originator marker (used by
/// running-process for crash-resilient orphan discovery) is set to
/// `zccache-cli:<pid>` unless an outer tool has already claimed it.
#[cfg(not(windows))]
fn apply_cli_spawn_lineage(cmd: &mut std::process::Command) {
    for (k, v) in cli_spawn_lineage_env() {
        cmd.env(k, v);
    }
}

/// Compute the lineage env-var pairs the CLI sets on the daemon it
/// spawns. Returns the same overrides `apply_cli_spawn_lineage` writes
/// onto a `Command`, in a form usable by the Windows raw-spawn path
/// (which needs to build its own merged environment block).
fn cli_spawn_lineage_env() -> Vec<(String, String)> {
    const ENV_ORIGINATOR: &str = "RUNNING_PROCESS_ORIGINATOR";
    const ENV_LINEAGE: &str = "ZCCACHE_LINEAGE";
    const ENV_PARENT_PID: &str = "ZCCACHE_PARENT_PID";
    const ENV_CLIENT_PID: &str = "ZCCACHE_CLIENT_PID";

    let cli_pid = std::process::id();
    let mut out: Vec<(String, String)> = Vec::with_capacity(4);

    // Preserve any outer originator (e.g. the build tool was already wrapped
    // by running-process). Otherwise, claim the originator slot ourselves.
    if std::env::var(ENV_ORIGINATOR).is_err() {
        out.push((ENV_ORIGINATOR.to_string(), format!("zccache-cli:{cli_pid}")));
    }

    // Extend or initialize the chain with our PID.
    let chain = match std::env::var(ENV_LINEAGE) {
        Ok(existing)
            if existing
                .rsplit_once('>')
                .map_or(existing.as_str(), |(_, last)| last)
                != cli_pid.to_string() =>
        {
            format!("{existing}>{cli_pid}")
        }
        Ok(existing) => existing,
        Err(_) => cli_pid.to_string(),
    };
    out.push((ENV_LINEAGE.to_string(), chain));
    out.push((ENV_PARENT_PID.to_string(), cli_pid.to_string()));
    out.push((ENV_CLIENT_PID.to_string(), cli_pid.to_string()));
    out
}

/// File name the daemon binary is deployed under. The daemon runs from a copy
/// of the CLI (self) placed under the versioned cache dir with the daemon's own
/// name, so argv[0] dispatch (#998) routes the copy to the daemon and
/// `verify_pid_exe_stem(pid, "zccache-daemon")` (zccache-ipc) recognizes it.
fn deployed_daemon_file_name() -> &'static str {
    if cfg!(windows) {
        "zccache-daemon.exe"
    } else {
        "zccache-daemon"
    }
}

/// Path the daemon binary is materialized to:
/// `<versioned cache dir>/zccache-daemon[.exe]` — e.g.
/// `~/.zccache/v<VERSION>/zccache-daemon.exe`.
///
/// Stable, version-rooted, using the daemon's own name (issue #999). Because
/// each installed version owns its own `v<VERSION>/` directory, a stale copy
/// from an older install can never masquerade as a newer one — this is the
/// structural fix for the #760 "soft-shadow" downgrade the old random-name
/// `runtime-binaries/` copies allowed.
#[must_use]
pub fn deployed_daemon_path() -> NormalizedPath {
    crate::core::config::daemon_state_dir().join(deployed_daemon_file_name())
}

/// Materialize the daemon binary at [`deployed_daemon_path`] by copying
/// `source` — the running CLI (`current_exe()`), which contains the daemon
/// via argv[0] dispatch.
///
/// **Idempotent**: if the destination already exists and its *contents* hash
/// equal to the source, it is reused unchanged, so N concurrent same-version
/// CLIs converge on one file with no repeated multi-MB copies. **Atomic**: the
/// copy lands on a temp name in the same directory and is `rename`d into
/// place, so no reader ever executes a torn binary; a concurrent materializer
/// that wins the rename is tolerated (we drop our temp and use theirs).
///
/// #1172: the gate used to be `len() == len()`. Size is not integrity — a file
/// of the right length with the wrong bytes was reused and executed as the
/// daemon, and this path runs on every spawn against a well-known location.
/// Hashing needs no build-time constant here because the daemon *is* a copy of
/// the running CLI (multi-call binary, `argv[0]` dispatch), so the source is
/// its own reference.
pub fn materialize_daemon_exe(source: &Path) -> Result<std::path::PathBuf, std::io::Error> {
    let dest = deployed_daemon_path().as_path().to_path_buf();
    materialize_daemon_exe_to(source, &dest)
}

/// Do two files have byte-identical contents?
///
/// Streamed rather than read-to-end: the daemon binary is tens of megabytes
/// and this runs on the spawn path. A read error answers `false`, which routes
/// the caller into the copy-and-rename path — the safe direction, since the
/// alternative is executing a file we could not verify.
fn files_have_equal_contents(a: &Path, b: &Path) -> bool {
    match (hash_file(a), hash_file(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn hash_file(path: &Path) -> std::io::Result<[u8; 32]> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(*hasher.finalize().as_bytes())
}

/// Create the directory the daemon binary is deployed into, owner-only.
///
/// #1172: anything that can write here chooses what the CLI executes as the
/// daemon. `ensure_dir_private` tightens a group/other-writable directory and
/// errors when it cannot; the refusal is loud on both surfaces, because
/// "someone else can write your daemon's directory" is not a detail to swallow.
fn ensure_deploy_dir_private(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    match crate::core::config::ensure_dir_private(dir) {
        Ok(false) => Ok(()),
        Ok(true) => {
            tracing::warn!(
                event = "insecure_deploy_dir",
                path = %dir.display(),
                outcome = "tightened",
                // Deliberately states what was observed, not the worst case it
                // could imply. On Windows this also fires the first time a
                // version directory is seen, because a directory inheriting an
                // otherwise-narrow profile DACL is not *protected* — nobody
                // else could necessarily write it. Claiming "another user could
                // have replaced your daemon" there would be a false alarm, and
                // false alarms are how a loud-forensics convention stops being
                // read.
                "daemon deploy directory was not restricted to this user and has been \
                 tightened; anything able to write it could choose what the CLI executes \
                 as the daemon"
            );
            crate::core::lifecycle::write_event(
                crate::core::lifecycle::EVENT_INSECURE_DEPLOY_DIR,
                serde_json::json!({
                    "path": dir.display().to_string(),
                    "outcome": "tightened",
                }),
            );
            Ok(())
        }
        Err(err) => {
            tracing::error!(
                event = "insecure_deploy_dir",
                path = %dir.display(),
                outcome = "refused",
                "refusing to deploy the daemon binary: {err}"
            );
            crate::core::lifecycle::write_event(
                crate::core::lifecycle::EVENT_INSECURE_DEPLOY_DIR,
                serde_json::json!({
                    "path": dir.display().to_string(),
                    "outcome": "refused",
                    "detail": err.to_string(),
                }),
            );
            Err(err)
        }
    }
}

/// Test seam for [`materialize_daemon_exe`]: materialize `source` at `dest`.
pub fn materialize_daemon_exe_to(
    source: &Path,
    dest: &Path,
) -> Result<std::path::PathBuf, std::io::Error> {
    // Integrity gate (#1172): an existing dest is reused only when its bytes
    // hash equal to the source. Size alone let a same-length, different-content
    // file be executed as the daemon. Size is still checked first as a cheap
    // reject so the common "already correct" path hashes once, not twice.
    if let (Ok(dm), Ok(sm)) = (std::fs::metadata(dest), std::fs::metadata(source)) {
        if dm.is_file() && dm.len() == sm.len() && files_have_equal_contents(source, dest) {
            return Ok(dest.to_path_buf());
        }
    }
    if let Some(parent) = dest.parent() {
        // Owner-only: this directory holds a binary the CLI will execute, so
        // anything that can write here can choose what the daemon runs. The
        // helper refuses rather than silently accepting a group/other-writable
        // directory it cannot tighten.
        ensure_deploy_dir_private(parent)?;
    }
    // Temp name in the SAME dir so the finalizing rename stays on one
    // filesystem (atomic). Unique per process so racing materializers don't
    // clobber each other's temp.
    let rand_id: u32 = std::process::id()
        ^ std::time::UNIX_EPOCH
            .elapsed()
            .unwrap_or_default()
            .subsec_nanos();
    let tmp = dest.with_file_name(format!("zccache-daemon.tmp.{rand_id}"));
    std::fs::copy(source, &tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755));
    }
    match std::fs::rename(&tmp, dest) {
        Ok(()) => Ok(dest.to_path_buf()),
        Err(e) => {
            // A concurrent materializer may have won the rename (or Windows is
            // refusing to replace a dest another process just created). Drop
            // our temp; if a usable dest now exists, use it.
            let _ = std::fs::remove_file(&tmp);
            if dest.is_file() {
                Ok(dest.to_path_buf())
            } else {
                Err(e)
            }
        }
    }
}

/// Subdir of the global cache directory where the daemon writes its own
/// stdout + stderr on every spawn. Each spawn gets a fresh file named
/// `daemon-spawn-{pid}-{nanos}.log` so concurrent CLI invocations don't
/// stomp each other. Errors that hit the daemon before its panic hook or
/// lifecycle log are alive land here — previously they went to `/dev/null`
/// on Unix and caused silent failures (notably the macOS regression that
/// motivated this change).
const DAEMON_SPAWN_LOGS_SUBDIR: &str = "logs";

/// Allocate a unique per-spawn log path under `{cache_dir}/logs/`.
/// The directory is created lazily; if creation fails we still hand back a
/// path — the daemon's own opener will see the error and fall back to
/// `Stdio::null` after warning.
fn allocate_daemon_spawn_log_path() -> std::path::PathBuf {
    let dir = crate::core::config::daemon_state_dir().join(DAEMON_SPAWN_LOGS_SUBDIR);
    let _ = std::fs::create_dir_all(dir.as_path());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id();
    let file_name = match crate::core::config::daemon_namespace() {
        Some(namespace) => format!("daemon-spawn-{namespace}-{pid}-{nanos}.log"),
        None => format!("daemon-spawn-{pid}-{nanos}.log"),
    };
    dir.as_path().join(file_name)
}

/// Default age cutoff for entries swept by [`gc_log_directory`]. Files
/// older than this are removed. Subdirectories are skipped (the daemon
/// doesn't create any under `logs/` today).
const LOG_GC_CUTOFF: std::time::Duration = std::time::Duration::from_secs(60 * 60 * 24);

/// Best-effort sweep of stale files in `{cache_dir}/logs/`.
///
/// Catches every log type that lands in this directory — not just
/// `daemon-spawn-*.log`. As of the issue-#323 fix this includes:
///   * `daemon-spawn-{pid}-{nanos}.log` (per-spawn daemon stdio
///     capture; CLI-owned)
///   * `daemon-lifecycle.log.1` (rotated lifecycle archive; the daemon
///     handles its own 1 MiB soft-cap but never garbage-collects the
///     archive, so it can sit on disk forever after the daemon exits)
///   * `daemon.log.*` (legacy rotated event-log archives; nothing writes
///     these since the unused `EventLogger` was removed in #1165, so the
///     sweep is now purely about reclaiming what older daemons left)
///   * `compile_journal.jsonl.*` (rotated compile-journal archives;
///     same rationale)
///   * Anything else that may have accumulated here from past versions
///     or external tooling
///
/// The active `daemon-lifecycle.log` is intentionally *preserved* — a
/// long-idle daemon may go 24h between writes (spawn → next event),
/// and deleting it mid-life would erase the very history that #323
/// needed to diagnose the multi-spawn bug.
pub fn gc_log_directory() {
    let dir = crate::core::config::daemon_state_dir().join(DAEMON_SPAWN_LOGS_SUBDIR);
    gc_log_directory_in(dir.as_path(), LOG_GC_CUTOFF);
}

/// Test seam for [`gc_log_directory`]. Sweeps stale files in `dir`
/// older than `cutoff`, preserving the active
/// `daemon-lifecycle.log` regardless of age.
pub fn gc_log_directory_in(dir: &Path, cutoff: std::time::Duration) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        // Skip the live lifecycle log: it's the one file that may sit
        // untouched between a daemon's `spawn` and `died-*` events.
        // Every other file in `logs/` either rotates often or is a
        // historical artifact safe to discard once old.
        if crate::core::lifecycle::is_live_lifecycle_log_name(&name) {
            continue;
        }
        let file_type = entry.file_type();
        if file_type.map(|t| !t.is_file()).unwrap_or(true) {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok());
        if let Some(age) = modified {
            if age > cutoff {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Back-compat alias for the broadened sweep. Earlier callers used
/// the spawn-log-only name; new code should use [`gc_log_directory`].
#[deprecated(note = "use gc_log_directory instead — sweeps the full logs/ directory")]
pub fn gc_daemon_spawn_logs() {
    gc_log_directory();
}

pub fn spawn_daemon(endpoint: &str) -> Result<(), String> {
    // Issue #982: backstop for the host no-spawn guard — refuse before
    // `materialize_daemon_exe` copies anything, so a guarded run leaves zero
    // daemon artifacts on disk.
    if crate::core::config::daemon_spawn_disabled() {
        return Err(crate::core::config::no_spawn_error("zccache-daemon"));
    }
    // GC old spawn logs (the runtime-binaries dir is gone — the daemon binary
    // is now a single stable version-rooted copy, pruned per-version by
    // `zccache clear`, #1005).
    gc_log_directory();

    // #999: the daemon is a copy of THIS binary (the CLI, which contains the
    // daemon via argv[0] dispatch) placed at the stable version-rooted path.
    // Copying from the install path means the install path is never
    // file-locked by a running daemon (the daemon runs from the copy), so
    // `pip install --upgrade zccache` / `rm -rf <project>` still succeed
    // (issue #134). Fall back to spawning the current exe in place if the
    // copy fails — the daemon's own `unlock_exe()` then handles the rename.
    let self_exe = std::env::current_exe()
        .map_err(|e| format!("cannot resolve current executable to deploy daemon: {e}"))?;
    let bin_owned: std::path::PathBuf;
    // `spawned_as_daemon` is true when we run the materialized copy, whose
    // argv[0] file stem is `zccache-daemon` so #998's dispatch routes it to
    // the daemon. On the fallback we run THIS exe in place (argv[0] =
    // `zccache`), which dispatches to the CLI — so we must enter the daemon
    // via the explicit `daemon-run` escape hatch instead.
    let (spawn_bin, spawned_as_daemon): (&Path, bool) = match materialize_daemon_exe(&self_exe) {
        Ok(p) => {
            bin_owned = p;
            (&bin_owned, true)
        }
        Err(_) => (self_exe.as_path(), false),
    };

    // Allocate a per-spawn log file path. Passed to the daemon via
    // `--log-file`; the daemon reopens its own stdout + stderr onto that
    // path early in startup. This replaces the previous Unix
    // `Stdio::null()` daemon spawn which made macOS dyld/gatekeeper
    // failures invisible (see PR #312 for full diagnosis).
    let log_path = allocate_daemon_spawn_log_path();
    let log_arg = log_path.to_string_lossy().into_owned();

    // Delegate the actual spawn to `running_process::spawn_daemon`
    // (renamed from `sanitized::spawn` in the 3.2 → 3.3 reshape — same
    // semantics, lives in the `spawn` module now and is re-exported at
    // the crate root). That helper handles both platform-specific quirks
    // the daemon hits:
    //  • Windows: STARTUPINFOEX + PROC_THREAD_ATTRIBUTE_HANDLE_LIST so
    //    grandparent pipe handles (e.g. Python's
    //    `subprocess.Popen(stdout=PIPE)` further up the chain) don't
    //    leak into the daemon and prevent EOF on the parent's read.
    //  • Unix: `setsid()` to detach from the controlling tty + close every
    //    fd > 2 between fork and exec so the same orphan-handle issue
    //    doesn't bite on macOS in particular.
    //
    // `DaemonChild` always opens NUL for its stdio at the spawn site;
    // the daemon then redirects its own stdout + stderr to `--log-file`
    // once it's running.
    let mut cmd = std::process::Command::new(spawn_bin);
    // On the fallback (running this exe in place), route into the daemon via
    // the argv[0]-independent `daemon-run` escape hatch (#998); the
    // materialized copy needs no subcommand because argv[0] already selects
    // the daemon.
    if !spawned_as_daemon {
        cmd.arg("daemon-run");
    }
    cmd.args([
        "--foreground",
        "--endpoint",
        endpoint,
        "--log-file",
        &log_arg,
    ]);
    #[cfg(not(windows))]
    apply_cli_spawn_lineage(&mut cmd);
    #[cfg(windows)]
    {
        // On Windows the sanitized spawn rebuilds the environment block
        // itself; pass our lineage overrides via `cmd.env(...)` so they
        // land in the merged block.
        for (k, v) in cli_spawn_lineage_env() {
            cmd.env(k, v);
        }
    }
    running_process::spawn_daemon(&mut cmd)
        .map(|_child| ())
        .map_err(|e| format!("failed to spawn daemon (sanitized): {e}"))
}

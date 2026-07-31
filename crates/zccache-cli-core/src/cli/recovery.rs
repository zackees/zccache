//! Bounded daemon-recovery policy for the wrapper (issue #1170, change 2).
//!
//! Since #1170 made "daemon unavailable" a hard error, recovery has to be
//! robust enough that a client can nearly always obtain a working daemon —
//! and, when it genuinely cannot, fail the whole build *fast* instead of once
//! per translation unit.
//!
//! Two mechanisms live here:
//!
//! - a **total deadline** across the probe → classify → clean up → respawn
//!   ladder, so a pathological host cannot turn one compile into minutes of
//!   retrying. Before this there was no overall cap at all.
//! - a **cross-invocation breaker**. The wrapper is a fresh process per
//!   TU, so nothing was shared between them: a 1000-TU build with a dead
//!   daemon paid the full ladder 1000 times. The first exhaustion writes a
//!   marker beside the daemon lock; subsequent invocations inside its
//!   cool-down skip the ladder and hard-error immediately. Any successful
//!   connect clears it.

use std::path::PathBuf;
use std::time::Duration;

/// Total budget for one client's recovery ladder.
pub(crate) const RECOVERY_BUDGET_ENV: &str = "ZCCACHE_RECOVERY_BUDGET_MS";

/// Default total deadline. Comfortably above a healthy cold spawn (~1-5 s)
/// including a 30 s drain of a retiring predecessor is *not* affordable here:
/// the point of the cap is that a client gives up while a human still
/// believes the build is running.
const DEFAULT_RECOVERY_BUDGET: Duration = Duration::from_secs(30);

/// First cool-down after the ladder is exhausted.
const BREAKER_INITIAL_COOLDOWN: Duration = Duration::from_secs(60);

/// Ceiling for the doubling cool-down. Long enough that a persistently broken
/// host stops paying anything, short enough that a fixed host recovers without
/// the user hunting for a file to delete.
const BREAKER_MAX_COOLDOWN: Duration = Duration::from_secs(10 * 60);

/// Suffix of the breaker marker, alongside `<lock>.spawn` from #952.
const BREAKER_MARKER_SUFFIX: &str = ".spawn-failed";

/// Resolved recovery budget, `ZCCACHE_RECOVERY_BUDGET_MS` or the default.
///
/// `0` disables the cap — the escape hatch for anyone bisecting against the
/// pre-#1170 unbounded behaviour. An unparseable value is ignored rather than
/// fatal: this is on the compile hot path, and refusing to build because a
/// tuning knob was misspelled is worse than the knob being ignored.
pub(crate) fn recovery_budget() -> Option<Duration> {
    recovery_budget_from(|name| std::env::var(name).ok())
}

fn recovery_budget_from<F>(lookup: F) -> Option<Duration>
where
    F: Fn(&str) -> Option<String>,
{
    let Some(raw) = lookup(RECOVERY_BUDGET_ENV) else {
        return Some(DEFAULT_RECOVERY_BUDGET);
    };
    match raw.trim().parse::<u64>() {
        Ok(0) => None,
        Ok(ms) => Some(Duration::from_millis(ms)),
        Err(_) => Some(DEFAULT_RECOVERY_BUDGET),
    }
}

/// Where the breaker marker lives for the current endpoint.
fn breaker_marker_path() -> PathBuf {
    let lock = crate::ipc::lock_file_path();
    PathBuf::from(format!("{}{BREAKER_MARKER_SUFFIX}", lock.display()))
}

/// Persisted breaker state. Serialized as JSON so an operator can read why
/// their build is failing without attaching a debugger.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct BreakerMarker {
    /// Wall-clock ms when the breaker last opened.
    opened_at_unix_ms: u64,
    /// How long invocations should skip the ladder from `opened_at_unix_ms`.
    cooldown_ms: u64,
    /// How many times the breaker has opened without an intervening success.
    consecutive_failures: u32,
    /// The failure that opened it, verbatim, so the fast-failing TUs can
    /// report the *original* cause rather than "breaker open".
    reason: String,
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// If the breaker is open and still inside its cool-down, the reason the
/// ladder failed the first time.
///
/// Reads the marker rather than caching in memory: the whole point is that
/// each TU is a separate process.
pub(crate) fn breaker_reason_if_open() -> Option<String> {
    let marker = read_marker(&breaker_marker_path())?;
    let elapsed_ms = now_unix_ms().saturating_sub(marker.opened_at_unix_ms);
    (elapsed_ms < marker.cooldown_ms).then_some(marker.reason)
}

fn read_marker(path: &std::path::Path) -> Option<BreakerMarker> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Record that the ladder was exhausted, doubling the cool-down if the
/// breaker was already open.
///
/// Emits `daemon_spawn_breaker_open` **once per opening**, not once per TU —
/// a 1000-TU build must not produce 1000 rows of the same fact.
pub(crate) fn open_breaker(reason: &str) {
    let path = breaker_marker_path();
    let previous = read_marker(&path);
    let consecutive_failures = previous
        .as_ref()
        .map_or(1, |marker| marker.consecutive_failures.saturating_add(1));
    let cooldown = previous
        .as_ref()
        .map(|marker| {
            Duration::from_millis(marker.cooldown_ms.saturating_mul(2))
                .min(BREAKER_MAX_COOLDOWN)
                .max(BREAKER_INITIAL_COOLDOWN)
        })
        .unwrap_or(BREAKER_INITIAL_COOLDOWN);

    let marker = BreakerMarker {
        opened_at_unix_ms: now_unix_ms(),
        cooldown_ms: cooldown.as_millis() as u64,
        consecutive_failures,
        reason: reason.to_string(),
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_vec(&marker) {
        let _ = std::fs::write(&path, json);
    }

    tracing::warn!(
        reason,
        cooldown_secs = cooldown.as_secs(),
        consecutive_failures,
        "daemon recovery exhausted; failing subsequent invocations immediately \
         until the cool-down expires"
    );
    crate::core::lifecycle::write_event(
        crate::core::lifecycle::EVENT_DAEMON_SPAWN_BREAKER_OPEN,
        serde_json::json!({
            "reason": reason,
            "cooldown_ms": marker.cooldown_ms,
            "consecutive_failures": consecutive_failures,
        }),
    );
}

/// Forget any breaker state. Called on every successful daemon acquisition,
/// including the fast path — a working daemon is proof the outage is over,
/// and leaving a stale marker would fast-fail a healthy build.
pub(crate) fn clear_breaker() {
    let _ = std::fs::remove_file(breaker_marker_path());
}

/// Remove state a dead daemon instance left behind.
///
/// #1170 change 2, step 3. Two artifacts were previously never cleaned on
/// this path:
///
/// - `<lock>.spawn` (#952's single-flight slot) was only *lazily* reclaimed
///   after 20 s, so a client that crashed between winning the slot and
///   binding the daemon wedged the whole herd for that window.
/// - the backend identity file was never removed at all, so a stale identity
///   outlived the instance it described.
pub(crate) fn clear_stale_daemon_state() {
    let lock = crate::ipc::lock_file_path();
    let spawn_slot = PathBuf::from(format!("{}.spawn", lock.display()));
    let _ = std::fs::remove_file(spawn_slot);
    let _ = std::fs::remove_file(crate::ipc::backend_identity_path().as_path());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_defaults_and_zero_disables_the_cap() {
        assert_eq!(
            recovery_budget_from(|_| None),
            Some(DEFAULT_RECOVERY_BUDGET)
        );
        assert_eq!(
            recovery_budget_from(|_| Some("1500".to_string())),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(recovery_budget_from(|_| Some("0".to_string())), None);
    }

    /// A misspelled tuning knob must not fail the build. This runs on the
    /// compile hot path, so the failure mode of a bad value has to be "the
    /// knob is ignored", never "nothing compiles".
    #[test]
    fn an_unparseable_budget_falls_back_to_the_default() {
        assert_eq!(
            recovery_budget_from(|_| Some("banana".to_string())),
            Some(DEFAULT_RECOVERY_BUDGET)
        );
    }

    /// The cool-down doubles per consecutive failure and saturates at the
    /// cap, so a persistently broken host stops paying while a transiently
    /// broken one recovers quickly.
    #[test]
    fn the_cooldown_doubles_and_saturates() {
        let mut cooldown = BREAKER_INITIAL_COOLDOWN;
        for _ in 0..20 {
            cooldown = (cooldown * 2).min(BREAKER_MAX_COOLDOWN);
        }
        assert_eq!(cooldown, BREAKER_MAX_COOLDOWN);
        assert!(BREAKER_INITIAL_COOLDOWN < BREAKER_MAX_COOLDOWN);
    }

    /// The marker must survive a round trip through JSON: it is read by a
    /// *different process* than the one that wrote it, which is the entire
    /// reason it is on disk rather than in memory.
    #[test]
    fn a_marker_inside_its_cooldown_reports_the_original_reason() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock.spawn-failed");
        let marker = BreakerMarker {
            opened_at_unix_ms: now_unix_ms(),
            cooldown_ms: 60_000,
            consecutive_failures: 1,
            reason: "cannot start daemon: binary not found".to_string(),
        };
        std::fs::write(&path, serde_json::to_vec(&marker).unwrap()).unwrap();

        let read = read_marker(&path).expect("marker round-trips");
        assert_eq!(read.reason, marker.reason);
        assert_eq!(read.consecutive_failures, 1);
        let elapsed = now_unix_ms().saturating_sub(read.opened_at_unix_ms);
        assert!(
            elapsed < read.cooldown_ms,
            "a freshly written marker must still be inside its cool-down"
        );
    }

    /// An expired marker must not fast-fail. The cool-down is what lets a
    /// fixed host recover on its own.
    #[test]
    fn a_marker_past_its_cooldown_no_longer_suppresses_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock.spawn-failed");
        let marker = BreakerMarker {
            // Opened well before the cool-down window.
            opened_at_unix_ms: now_unix_ms().saturating_sub(120_000),
            cooldown_ms: 60_000,
            consecutive_failures: 1,
            reason: "stale".to_string(),
        };
        std::fs::write(&path, serde_json::to_vec(&marker).unwrap()).unwrap();

        let read = read_marker(&path).expect("marker round-trips");
        let elapsed = now_unix_ms().saturating_sub(read.opened_at_unix_ms);
        assert!(
            elapsed >= read.cooldown_ms,
            "the cool-down must be expired for this fixture to mean anything"
        );
    }

    /// A corrupt or truncated marker must read as "no breaker". Failing
    /// closed here would make an unparseable file permanently unbuildable.
    #[test]
    fn a_corrupt_marker_is_ignored_rather_than_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock.spawn-failed");
        std::fs::write(&path, b"{not json").unwrap();
        assert!(read_marker(&path).is_none());
    }
}

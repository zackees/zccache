//! Perf unit test (#1649): an idle daemon costs (almost) no CPU.
//!
//! kernal-api's native crash capture runs a pre-crash sampler that takes a
//! full resolved all-thread snapshot every 50 ms for the life of the process.
//! The daemon lives for the whole build and beyond, so with native capture
//! armed an *idle* daemon burned 85-90% of a core indefinitely (the sampler
//! thread accounted for 359 s of CPU in a 7-minute idle window). It also took
//! that core away from the compilers during every build.
//!
//! Linux only: the measurement sums `/proc/<pid>/task/*/schedstat`, which
//! reports on-CPU time in nanoseconds for every thread of the daemon.

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Let start-up work (index load, first maintenance tick) finish first.
const SETTLE: Duration = Duration::from_millis(1500);
const WINDOW: Duration = Duration::from_secs(3);
/// An idle daemon should be asleep in epoll. 15% of one core leaves room for
/// periodic maintenance on a slow runner; the sampler alone used 85-90%.
const IDLE_CPU_BUDGET: f64 = 0.15;

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Total on-CPU nanoseconds across every thread of `pid`.
fn cpu_ns(pid: u32) -> u64 {
    let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).expect("read daemon tasks");
    tasks
        .flatten()
        .filter_map(|task| std::fs::read_to_string(task.path().join("schedstat")).ok())
        .filter_map(|stat| stat.split_whitespace().next()?.parse::<u64>().ok())
        .sum()
}

fn wait_for_listening(log_file: &Path) {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while Instant::now() < deadline {
        let log = std::fs::read_to_string(log_file).unwrap_or_default();
        if log.contains("listening for connections") {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "daemon never reached listening state; log: {}",
        std::fs::read_to_string(log_file).unwrap_or_default()
    );
}

#[test]
fn idle_daemon_stays_within_cpu_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = tmp.path().join("cache");
    std::fs::create_dir_all(&cache_dir).unwrap();
    let log_file = tmp.path().join("daemon.log");
    let endpoint = tmp.path().join("idle-cpu.sock");

    let child = Command::new(env!("CARGO_BIN_EXE_zccache-daemon"))
        .args(["--foreground", "--endpoint"])
        .arg(&endpoint)
        .arg("--log-file")
        .arg(&log_file)
        .args(["--idle-timeout", "60"])
        .env("ZCCACHE_CACHE_DIR", &cache_dir)
        .env("ZCCACHE_DAEMON_NAMESPACE", "idle-cpu-budget")
        .env("ZCCACHE_QUIET", "1")
        .env("ZCCACHE_NO_UNLOCK", "1")
        .env_remove("ZCCACHE_NATIVE_CRASH_CAPTURE")
        .env_remove("KERNAL_API_NO_CRASH_HANDLER")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn zccache-daemon");
    let daemon = Daemon(child);
    let pid = daemon.0.id();

    wait_for_listening(&log_file);
    std::thread::sleep(SETTLE);

    let started = Instant::now();
    let before = cpu_ns(pid);
    std::thread::sleep(WINDOW);
    let used = cpu_ns(pid).saturating_sub(before);
    let wall = started.elapsed();

    let fraction = used as f64 / wall.as_nanos() as f64;
    eprintln!("idle daemon used {used} ns of CPU over {wall:?} ({fraction:.3} of a core)");
    assert!(
        fraction <= IDLE_CPU_BUDGET,
        "idle daemon used {:.0}% of a core over {wall:?} (budget {:.0}%): a steady \
         background cost is back in the daemon (#1649)",
        fraction * 100.0,
        IDLE_CPU_BUDGET * 100.0
    );
}

# #968 wedge/timeout burn-down — near complete

Meta: https://github.com/zackees/zccache/issues/968

## Merged to main ✅
- #967 (PR #969) client-disconnect cancellation
- #962 (PR #970) orphan-pipe post-exit watchdog (Mode A)
- #971 (PR #976) in_flight_exec lost-wakeup + bounded waiter + exec-spawn watchdog
- #972 (PR #978) `-vV` identity probe timeout
- #973 (PR #977) embedded flush disk-save bounds
- #891 (PR #980) CPU/output progress watchdog (Mode B) — all-platform CPU sampling
- #890 (PR #981) async/process bridge design doc (runtime.md)

## In CI (merge when green) 🔁
- #974 (PR #979) watcher consumer wakes on shutdown
- #893 (PR #983) child pid in watchdog diagnostics
- #892 (PR #984) pipe-saturation / concurrent-drain regression test

## Remaining 📋
- #894 concurrency-not-reduced test (child_watchdog tests) — DO AFTER #892 merges
  (same tests-module region → conflict otherwise). N concurrent watchdog waits on
  ~1s sleepers → assert total < serial (not serialized).
- Then close #889 (all children #890/#891/#892/#893/#894 done) + close #968.

## Extra issues filed (user requests)
- #975 internal multi-crate split for parallel compiles (single published crate)
- soldr#1465 soldr build wedge (embedded zccache cache) — silent death; ZCCACHE_DISABLE=1 recovery

## Key techniques learned (memory)
- ZCCACHE_DISABLE=1 to bypass wedge-prone build cache locally
- soldr cargo check --target <triple> to validate cfg(linux)/cfg(macos) arms locally
- Always `cargo fmt --all` (auto-fix) + real exit check (no pipe mask) before commit
- Kill orphan cargo.exe holding target/debug/.cargo-lock if builds "block"

## Design rules honored
- Progress/CPU-based watchdogs, never dumb wall-clock (links run minutes)
- Every timeout/watchdog fire: loud warn! + durable lifecycle event (forensics)
- Constants at top of file

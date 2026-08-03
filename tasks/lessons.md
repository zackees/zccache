# Lessons

## macOS CI is queue-bound, not compute-bound — cut job *count*, not job duration (2026-07-28)

Measured on macOS run `30378201671`: four jobs queued 16/32/49/59 min and ran
43s/44s/3m26s/2m42s. **61.7 min wall clock, 7.5 min compute** — ~88% of macOS CI
time is waiting for a runner slot against GitHub's 5-concurrent-job macOS cap.

Two amplifiers were live at once: `ci-macos.yml` declared jobs `x86` and `arm`
that *both* pinned `os: macos-14` (byte-identical duplicates, and they derived
the same `cache-key-suffix` so they also raced on cache save), and ~24 of ~38 PR
jobs had no `concurrency` block, so superseded runs kept holding slots while a
fresh run queued behind them. That is what produces a monotone queue staircase.

**Rule:** when CI feels slow, compare `startedAt - createdAt` against
`completedAt - startedAt` per job before optimizing anything:
`gh run view <id> --json jobs -q '.jobs[] | "\(.name)\t\(.startedAt)\t\(.completedAt)"'`.
If queue dominates, the only lever that works is removing jobs from scarce pools
(macOS > windows-11-arm > everything else). Making a job faster barely moves a
number that is 88% waiting. `wrapper-e2e.yml` already recorded this once —
dropping one redundant `macos-13` leg was worth "15+ minutes of queue wait".

**Corollary — don't cross-compile to dodge this.** Building test binaries on
Linux and shipping them to mac/Windows runners was considered and rejected: it
does not reduce the number of queued jobs at all, the ~32 debug test binaries
are 600 MB–1.5 GB (5–15 min of artifact transfer to avoid ~60–90s of compile),
Linux→macOS needs the Xcode SDK on non-Apple hardware (license violation on a
public repo), and `build-target/action.yml` already hard-fails cross-built
`pc-windows-msvc` because of the #269 CRT-skew `STATUS_DLL_INIT_FAILED` class.

## Stopping a `soldr cargo` task can orphan its cargo child and wedge the build lock (2026-07-08)

`TaskStop` on a background `soldr cargo test/check` kills the bash wrapper but
the `cargo.exe` grandchild can survive as an orphan, keep holding the
`target/debug/.cargo-lock`, and make every *subsequent* build print
"Blocking waiting for file lock on build directory" — which looked like builds
"dying" (they were actually blocked forever, and my monitors' racy
no-process check misread it). The monolithic `zccache` crate's multi-minute
single-rustc compile (see #975) amplified the confusion.

**Rule:** don't launch overlapping `soldr cargo` builds, and don't `TaskStop`
one mid-compile and immediately start another. If a build wedges, run
`taskkill /F /IM cargo.exe //T; taskkill /F /IM rustc.exe //T` to clear orphans
before retrying, and confirm `tasklist | grep -c cargo` is 0. Prefer the lighter
`cargo check` locally and let CI run the heavy test build.

## Admin-merge only after the fast lint gates are green (2026-07-08)

**Mistake:** admin-merged #967 (PR #969) without waiting for CI. It carried two
lint regressions that turned main red:
- rustdoc `-D warnings`: a public item's doc (`wait_for_disconnect`) linked to a
  private item (`framing::read_next_chunk`). Public→private intra-doc links are a
  hard rustdoc error under `-D warnings`.
- rustfmt: an edited file was not `cargo fmt`-clean.

Local `cargo check -p <crate> --lib` and unit tests were green, so the code was
correct — but neither runs rustdoc or rustfmt. The Documentation and Formatting
CI jobs (in the Clippy workflow) caught it only after merge.

**Rule for the future:** before pushing/merging any PR, run the fast gates
locally — they're cheap and catch exactly what `cargo check` misses:
- `soldr cargo fmt --all --check`  (instant)
- `RUSTDOCFLAGS="-D warnings" soldr cargo doc -p <crate> --no-deps --lib`  (~1 min)
- `soldr cargo clippy -p <crate> --lib` (feature-gated integration tests need the
  right `--features`; use `--workspace --all-targets` to match CI's feature
  unification, or scope with the needed feature).

If admin-merging past CI for speed, at minimum run fmt --check + doc first. The
perf-rust-cluster jobs (`arm / Test`, `x86 / Test`, `arm-musl / Check`) are
known-broken at the pin step and are NOT merge-blocking — don't chase them.

## soldr is the bootstrapper even inside the docker Linux build container

When validating in the `clud-docker-build` soldr container, go through
`soldr cargo ...` / `soldr rustup ...`, NOT bare `cargo install` / `rustup`.
soldr is baked into the image (`/usr/local/bin/soldr`) and owns toolchain
discovery. Bare `cargo install cargo-dylint` failed with "rustup is not
installed at '/cargo-home'" because CARGO_HOME points at a named volume whose
proxies weren't seeded; `soldr cargo install` resolves the right toolchain
front-door. The host PreToolUse hook enforces this, but `docker exec` bypasses
the hook, so the discipline has to be applied manually in container scripts.
**Rule:** any container-side build/lint script uses `soldr <tool>`.

## A snapshot plus mutable delta is only read-fast under restricted semantics

For a strongly consistent general map, readers must check the mutable delta
before the immutable snapshot so updates and tombstones override old values.
That makes a steady-state hit pay a concurrent-map miss plus a snapshot lookup,
which is usually worse than one `DashMap` hit. Snapshot-first is valid for
append-only, immutable, unique keys, but removals must be published atomically
before backing data is deleted. Treat this as a specialized artifact-index
design, not a generic `DashMap` replacement.

Before replacing a concurrent map, also audit the guard lifetime. In the
artifact hit path, `get_mut` held an exclusive DashMap shard guard through lazy
payload filesystem resolution and retention checkpointing. Sharing those
mutable fields through per-entry cells now lets lookup clone an owned entry
under a short read guard, attacking contention with much less consistency and
merge machinery.

## Source immutability is request-scoped, not build-session-scoped

A daemon session can contain many compiler invocations, and generated sources,
headers, and Rust extern artifacts can legitimately appear or change between
them. It is reasonable to optimize for inputs remaining stable during one
compiler invocation, but cache publication must still fail closed if that
assumption is violated: otherwise a transient racy compile becomes a persistent
artifact under the wrong key. Keep a request-local owned hash snapshot; do not
pin a build-long immutable metadata generation.

Metadata GC is not an input mutation. Removing a metadata or journal entry only
forces a later stat/rehash because a missing journal entry is conservatively
treated as changed. A concurrent sweep can discard a freshly refreshed entry
due to the current snapshot-then-remove implementation, but that is a
performance race, not a correctness race.

## Pressure GC should mark, defer, and conditionally sweep

Separate metadata eviction into a shared-read candidate scan and a bounded
conditional purge. Each candidate carries the observed entry revision; the
purge rechecks that revision while holding the shard write lock and skips a
locked or refreshed entry. Defer the purge while whole cache requests are
active, then escalate from idle-only to nonblocking opportunistic batches and
finally a bounded blocking pass. The existing compiler-child counter is too
narrow because it misses cache hits, pre-hashing, and post-compile publication.

`last_verified` is not an LRU timestamp: the stat-verified journal fast path
does not refresh it. A GC policy that wants to preserve hot inputs needs a
separate approximate last-access epoch or CLOCK/reference bit, preferably
updated with a relaxed atomic rather than a DashMap write on every read.

## A short map guard still needs an explicit backing-data lease

Cloning an artifact out of a `DashMap` removes shard contention, but it also
removes the old implicit guarantee that disk maintenance cannot delete payload
files while a hit discovers or materializes them. Keep the owned artifact body
behind one `Arc` and pair lookup with a publication read lease. Maintenance and
Clear take the write side only for destructive work, which orders late access
WAL inserts before removal without holding a map shard.

Do not hold that global writer during a large read-only filesystem scan. Scan
and plan concurrently with hits, then acquire the writer, refresh live access,
re-plan, and commit deletion. Otherwise every warm lookup either waits or
becomes a cold compile for the duration of each maintenance scan.

## Nonblocking GC must distinguish contention from freshness

A failed `try_entry` says only that a shard is busy; it does not authorize
protecting that candidate for the whole sweep. Report busy and
timestamp-refreshed candidates separately. Forced completion retries busy
candidates with blocking entry locks, excludes refreshed paths from any
same-sweep re-plan, and selects replacement candidates until it reaches the
headroom target or only protected metadata remains. Also settle orphaned
journal rows explicitly: a gentle metadata deletion can otherwise leave memory
debt after the metadata entry itself is gone.

## Never validate a soldr command with a `^error`-anchored grep

Local `clippy --all-targets -- -D warnings` reported clean; CI then failed the
integration leg on `unused_must_use` in a test I had just edited. The lint was
in the local output the whole time. `soldr` prefixes every line with a timing
column (`   88.69 error: ...`) and emits ANSI colour, so an anchored
`grep -E "^(error|warning)"` matches nothing and a clean-looking filter is
indistinguishable from a clean build.

Judge a build by its **exit code**, not by a line filter:

```sh
soldr cargo clippy -p <crate> --all-targets -- -D warnings >/tmp/out.txt 2>&1
echo "exit=$?  diagnostics=$(grep -cE 'warning:|error(\[|:)' /tmp/out.txt)"
```

Two independent signals, neither anchored. Also export `RUSTFLAGS="-D warnings"`
to match CI: rustc lints like `unused_must_use` are denied there, and passing
`-- -D warnings` to clippy alone does not reproduce the same gate.

This was already recorded in memory as "Validating soldr output on Windows" and
I still repeated it, which is the actual lesson: the failure mode is silent and
looks exactly like success, so the filter has to be structurally incapable of
lying rather than merely correct on the day it was written.

## Removing a database can remove serialization you never knew you relied on

#1352 replaced `KvStore`'s redb backend with one file per key. The public API,
the CLI surface, and every functional test were preserved, and the unit tests
passed first try. The stress suite did not: `c1_thundering_herd_same_key`
(16 threads x 100 writes to one key) failed immediately with
`ERROR_ACCESS_DENIED` on Windows.

The cause was not in the new code. It was in what the old code had been doing
for free. Every write to a key went through one redb write transaction, so
same-key writers were **serialized by the database** as a side effect of
durability. File-per-key deletes that serialization — which is the entire point
of the change — but `MOVEFILE_REPLACE_EXISTING` cannot start while another
handle is open on the destination, so concurrent same-key renames collide.

The fix needed two mechanisms, and the first alone was not enough: a bounded
retry on the transient Windows sharing errors could not converge against 1,600
renames onto one path, because the destination is open essentially always. It
took sharded per-key mutexes held across the rename only. **I found that out by
running the test, not by reasoning about it** — the retry looked obviously
sufficient right up until it wasn't.

Lesson: when you remove a component, enumerate the *incidental* guarantees it
was providing, not just the advertised ones. A database gives you serialization,
ordering, and atomicity across keys whether or not you asked for them. Then
trust the concurrency tests over your model of the change: the functional tests
all passed while the design was still wrong.

## Docs that describe a removed subsystem are an active liability

A user reported `redb: Database already open. Cannot acquire lock.` The
investigation started in zccache and stayed there for a while, because
`crates/CLAUDE.md` said "redb MVCC for artifact index" and
`architecture/artifact-store.md` documented a two-table redb schema. Both had
been false since the index moved to a bincode blob. The error was in a different
repository entirely (soldr's `state.redb`).

Worse, soldr#1814 had already done exactly this audit months earlier and reached
the same conclusion. The stale docs did not just slow one investigation down —
they caused a completed one to be repeated from scratch.

Lesson: when a subsystem is replaced, the doc sweep is part of the change, not
cleanup to schedule later. Grep for the old technology's name across `docs/`,
`README.md`, and every `CLAUDE.md`, and check test/bench identifiers too —
`run_strategy_*_redb` names survived on functions that call the bincode store,
which would have misled the next person profiling persistence. Mark superseded
ADRs superseded rather than rewriting them; the reason a decision changed is
usually more useful than the decision.

# Lessons

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

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

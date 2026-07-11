# zccache

[![Linux](https://github.com/zackees/zccache/actions/workflows/ci-linux.yml/badge.svg)](https://github.com/zackees/zccache/actions/workflows/ci-linux.yml)
[![macOS](https://github.com/zackees/zccache/actions/workflows/ci-macos.yml/badge.svg)](https://github.com/zackees/zccache/actions/workflows/ci-macos.yml)
[![Windows](https://github.com/zackees/zccache/actions/workflows/ci-windows.yml/badge.svg)](https://github.com/zackees/zccache/actions/workflows/ci-windows.yml)
[![Perf Guard](https://github.com/zackees/zccache/actions/workflows/perf-guard.yml/badge.svg?branch=main)](https://github.com/zackees/zccache/actions/workflows/perf-guard.yml?query=branch%3Amain)
[![Clippy](https://github.com/zackees/zccache/actions/workflows/clippy.yml/badge.svg?branch=main)](https://github.com/zackees/zccache/actions/workflows/clippy.yml?query=branch%3Amain)
[![Dylint](https://img.shields.io/github/actions/workflow/status/zackees/zccache/ci.yml?branch=main&event=push&job=Dylint&label=dylint)](https://github.com/zackees/zccache/actions/workflows/ci.yml?query=branch%3Amain+event%3Apush)
[![codecov](https://codecov.io/gh/zackees/zccache/branch/main/graph/badge.svg)](https://codecov.io/gh/zackees/zccache)
[![PyPI](https://img.shields.io/pypi/v/zccache)](https://pypi.org/project/zccache/)
[![crates.io: zccache-core](https://img.shields.io/crates/v/zccache-core)](https://crates.io/crates/zccache-core)
[![crates.io: zccache-cli](https://img.shields.io/crates/v/zccache-cli)](https://crates.io/crates/zccache-cli)
[![crates.io: zccache-daemon](https://img.shields.io/crates/v/zccache-daemon)](https://crates.io/crates/zccache-daemon)
[![Rust Workspace Version](https://img.shields.io/badge/rust%20workspace-1.3.10-orange)](https://crates.io/search?q=zccache)
[![GitHub Action](https://github.com/zackees/zccache/actions/workflows/test-action.yml/badge.svg)](https://github.com/zackees/zccache/actions/workflows/test-action.yml)

![C/C++](https://img.shields.io/badge/C%2FC%2B%2B-555?logo=c%2B%2B&logoColor=white)
[![clang](https://img.shields.io/badge/clang-supported-brightgreen?logo=llvm)](https://clang.llvm.org/)
[![clang++](https://img.shields.io/badge/clang++-supported-brightgreen?logo=llvm)](https://clang.llvm.org/)
[![clang-tidy](https://img.shields.io/badge/clang--tidy-supported-brightgreen?logo=llvm)](https://clang.llvm.org/extra/clang-tidy/)
[![IWYU](https://img.shields.io/badge/IWYU-supported-brightgreen?logo=llvm)](https://include-what-you-use.org/)

![Rust](https://img.shields.io/badge/Rust-555?logo=rust&logoColor=white)
[![rustc](https://img.shields.io/badge/rustc-supported-brightgreen?logo=rust)](https://www.rust-lang.org/)
[![clippy](https://img.shields.io/badge/clippy-supported-brightgreen?logo=rust)](https://github.com/rust-lang/rust-clippy)
[![rustfmt](https://img.shields.io/badge/rustfmt-supported-brightgreen?logo=rust)](https://github.com/rust-lang/rustfmt)

![Emscripten](https://img.shields.io/badge/Emscripten-555?logo=webassembly&logoColor=white)
[![emcc](https://img.shields.io/badge/emcc-supported-brightgreen?logo=webassembly)](https://emscripten.org/)
[![em++](https://img.shields.io/badge/em++-supported-brightgreen?logo=webassembly)](https://emscripten.org/)
[![wasm-ld](https://img.shields.io/badge/wasm--ld-supported-brightgreen?logo=webassembly)](https://lld.llvm.org/WebAssembly.html)

### A blazing fast compiler cache for C/C++ and Rust


![New Project](https://github.com/user-attachments/assets/f4d85974-0772-4710-b9f8-47bbd9439cef)

Inspired by [sccache](https://github.com/mozilla/sccache), but optimized for local-first use
with aggressive file metadata caching and filesystem watching.

## Quick Install

```bash
curl -LsSf https://github.com/zackees/zccache/releases/latest/download/install.sh | sh
```

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/zackees/zccache/releases/latest/download/install.ps1 | iex"
```

Verify:

```bash
zccache --version
```

## Performance

[![Latest zccache C benchmark stats](https://raw.githubusercontent.com/zackees/zccache/benchmark-stats/benchmark-c.jpg)](https://github.com/zackees/zccache/tree/benchmark-stats)

[![Latest zccache C++ benchmark stats](https://raw.githubusercontent.com/zackees/zccache/benchmark-stats/benchmark-cpp.jpg)](https://github.com/zackees/zccache/tree/benchmark-stats)

[![Latest zccache Emscripten benchmark stats](https://raw.githubusercontent.com/zackees/zccache/benchmark-stats/benchmark-emscripten.jpg)](https://github.com/zackees/zccache/tree/benchmark-stats)

[![Latest zccache Rust benchmark stats](https://raw.githubusercontent.com/zackees/zccache/benchmark-stats/benchmark-rust.jpg)](https://github.com/zackees/zccache/tree/benchmark-stats)

The benchmark images are generated from the latest scheduled run and replace
hand-maintained text stats. Full results, rendered HTML, and machine-readable
JSON are published in the
[benchmark-stats branch](https://github.com/zackees/zccache/tree/benchmark-stats)
and at [zackees.github.io/zccache](https://zackees.github.io/zccache/). Run the
same suite locally with `./perf.sh`.

### Why is zccache so much faster on warm hits?

The difference comes from **architecture**, not better caching:

| | sccache | zccache |
|---|---------|---------|
| **IPC model** | Subprocess per invocation (fork + exec + connect) | Persistent daemon, single IPC message per compile |
| **Cache lookup** | Client hashes inputs, sends to server, server checks disk | Daemon has inputs in memory (file watcher + metadata cache) |
| **On hit** | Server reads artifact from disk, sends back via IPC | Daemon hardlinks cached file to output path (1 syscall) |
| **Multi-file** | Compiles every file (no multi-file cache support) | Parallel per-file cache lookups, only misses go to the compiler |
| **Per-hit cost** | ~170ms (process spawn + hash + disk I/O + IPC) | ~1ms (in-memory lookup + hardlink) |

**Architecture enhancements that make the difference:**

- **Filesystem watcher** — a background `notify` watcher tracks file changes in real time, so the daemon already knows whether inputs are dirty before you even invoke a compile. No redundant stat/hash work on hit.
- **In-memory metadata cache** — file sizes, mtimes, and content hashes live in a lock-free `DashMap`. Cache key computation is a memory lookup, not disk I/O.
- **Single-roundtrip IPC** — each compile is one length-prefixed daemon message over a Unix socket (or named pipe on Windows). No subprocess spawning, no repeated handshakes. The active wire is v15 bincode; a v16 prost wire is staged behind the migration tracked by zackees/running-process#234.
- **Hardlink delivery** — cache hits are served by hardlinking the cached artifact to the output path — a single syscall instead of reading + writing the file contents.
- **Multi-file fast path** — when a build system passes N source files in one invocation, zccache checks all N against the cache in parallel, serves hits immediately, and batches only the misses into a single compiler process.

### Feature comparison vs sccache

The full matrix lives at [`docs/FEATURE-MATRIX.md`](docs/FEATURE-MATRIX.md) and is generated from [`docs/feature-matrix.yaml`](docs/feature-matrix.yaml) — these tables are auto-rendered, do not edit them by hand.

<!-- BEGIN feature-matrix-headline -->
| Feature | zccache | sccache |
|---|:---:|:---:|
| Link caching (artifact + sibling files) | yes | no |
| Emscripten (emcc / em++) | yes | no |
| Multi-file compilation fast path | yes | no |
| clang-tidy (static analysis results cached) | yes | no |
| include-what-you-use (IWYU) | yes | no |
| Persistent daemon with sub-ms IPC | yes | partial |
| Safe hardlink delivery on hit | yes | no |
| ZCCACHE_PATH_REMAP=auto cross-worktree sharing | yes | no |
| GitHub Actions Cache (native API client) | yes | yes |
| Per-hit cost | yes | partial |
<!-- END feature-matrix-headline -->

<details>
<summary>Full feature comparison (zccache vs sccache)</summary>

See [`docs/FEATURE-MATRIX.md`](docs/FEATURE-MATRIX.md) for the long-form view with notes and evidence columns.

<!-- BEGIN feature-matrix-full -->
#### Caching scope

| Feature | zccache | sccache |
|---|:---:|:---:|
| C/C++ object caching | yes | yes |
| Link caching (artifact + sibling files) | yes | no |
| Rust rustc caching (--emit=metadata, --emit=link, extern crate hashing) | yes | yes |
| Emscripten (emcc / em++) | yes | no |
| wasm-ld linking | yes | no |
| CUDA / nvcc | no | yes |
| MSVC cl.exe / link.exe | partial | yes |
| Multi-file compilation fast path | yes | no |
| Response file (.rsp) expansion | yes | partial |

#### Tool coverage

| Feature | zccache | sccache |
|---|:---:|:---:|
| clang-tidy (static analysis results cached) | yes | no |
| include-what-you-use (IWYU) | yes | no |
| rustfmt | yes | no |
| clippy | yes | no |
| cargo check / cargo build | yes | yes |

#### Build system integration

| Feature | zccache | sccache |
|---|:---:|:---:|
| Ninja (via CC / CXX launcher) | yes | yes |
| CMake (CMAKE_C_COMPILER_LAUNCHER) | yes | yes |
| Meson (native file) | yes | yes |
| Make | yes | yes |
| RUSTC_WRAPPER | yes | yes |
| setuptools-rust / maturin / scikit-build-core | yes | yes |

#### Architecture

| Feature | zccache | sccache |
|---|:---:|:---:|
| Persistent daemon with sub-ms IPC | yes | partial |
| Single-roundtrip IPC (length-prefixed bincode) | yes | no |
| Safe hardlink delivery on hit | yes | no |
| Reflink delivery (ReFS, btrfs/XFS, APFS) | yes | no |
| In-memory metadata cache (DashMap) | yes | no |
| Filesystem watcher (notify-backed) | yes | no |
| Content-addressed artifact store | yes | yes |
| Protocol versioning (wire-format bump policy) | yes | partial |
| Compile journal (JSONL replay log) | yes | no |
| Session stats / per-build hit rates | yes | partial |
| Crash dumper (CLI + daemon) | yes | no |

#### Worktree / multi-checkout

| Feature | zccache | sccache |
|---|:---:|:---:|
| ZCCACHE_PATH_REMAP=auto cross-worktree sharing | yes | no |
| C/C++ -ffile-prefix-map injection | yes | no |
| Rust --remap-path-prefix injection | yes | no |
| Strict path validation | yes | no |

#### Storage backends

| Feature | zccache | sccache |
|---|:---:|:---:|
| Local filesystem | yes | yes |
| S3 | no | yes |
| Google Cloud Storage | no | yes |
| Redis | no | yes |
| Memcached | no | yes |
| Azure Blob | no | yes |
| GitHub Actions Cache (native API client) | yes | yes |
| Distributed scheduler / build farm | no | yes |

#### Platform / packaging

| Feature | zccache | sccache |
|---|:---:|:---:|
| Linux x86_64 | yes | yes |
| Linux aarch64 | yes | yes |
| macOS x86_64 | yes | yes |
| macOS arm64 (Apple Silicon) | yes | yes |
| Windows x86_64 | yes | yes |
| Windows arm64 | yes | partial |
| Windows Defender exclusion helper | yes | no |
| PyPI wheels | yes | no |
| crates.io publishing | yes | yes |
| GitHub Action (composite) | yes | yes |
| Target snapshot caching + zccache warm backfill | yes | no |

#### Performance posture

| Feature | zccache | sccache |
|---|:---:|:---:|
| Per-hit cost | yes | partial |
| mtime preservation on hits | yes | partial |
| Compiler child priority (auto-throttle at 95% CPU) | yes | no |

#### Reliability

| Feature | zccache | sccache |
|---|:---:|:---:|
| Hardlink cache-poisoning prevention and detection | yes | partial |
<!-- END feature-matrix-full -->

</details>

**Broader tool coverage** — zccache supports modes that other compiler caches don't:

| Mode | Description |
|------|-------------|
| **Multi-file compilation** | `clang++ -c a.cpp b.cpp c.cpp` — per-file caching with parallel lookups |
| **Response files** | Nested `.rsp` files with hundreds of flags — fully expanded and cached |
| **clang-tidy** | Static analysis results cached and replayed |
| **include-what-you-use** | IWYU output cached per translation unit |
| **Emscripten (emcc/em++)** | WebAssembly compilation cached end-to-end |
| **wasm-ld** | WebAssembly linking cached |
| **rustfmt** | Formatting results cached |
| **clippy** | Lint results cached |
| **Rust check & build** | `cargo check` and `cargo build` with extern crate content hashing |

### Link-Time Side Effects

Linker drivers sometimes create more than the named `-o` output. Windows and
MinGW toolchains may deploy runtime DLLs next to an executable, MSVC links may
emit PDB files, and Emscripten links may create `.wasm`, `.map`, or `.js`
sidecars. If a cache hit restored only the primary binary, those sibling files
could be missing even though the cached binary itself was correct.

For link invocations, zccache snapshots the output directory before running the
real linker, records sibling files created or changed by a successful link, and
stores them with the primary link artifact. Later cache hits restore the full
artifact set to the output directory, so tools such as `clang-tool-chain` runtime
deployment work without per-build-system post-link hooks.

### Windows: Defender exclusions

Windows Defender's real-time scanner inspects every freshly written file before
the write returns. zccache writes hundreds of `.rmeta` / `.rlib` / `.o` files per
cold build, and on an unexcluded cache directory each one pays the Defender
round-trip — multi-minute slowdowns are routine and invisible to zccache's own
telemetry (the daemon sees its writes complete normally; the wall clock balloons
between `write()` and the bytes reaching disk).

The fix is a one-time exclusion. zccache ships a helper so you don't have to
hand-craft the PowerShell:

```powershell
# Show whether the cache root is excluded. Read-only — no elevation needed.
zccache defender-exclusions check

# Add the exclusion. Requires an elevated PowerShell or Administrator cmd.
zccache defender-exclusions add

# Undo.
zccache defender-exclusions remove
```

On Windows, the daemon prints a one-line stderr warning at startup if the cache
directory isn't excluded yet. Silence it with `ZCCACHE_QUIET=1`. The
`defender-exclusions` subcommand is available on every platform — non-Windows
hosts print `Defender exclusion is Windows-only.` and exit 0 so cross-platform
scripts can call it unconditionally.

## Install

```bash
curl -LsSf https://github.com/zackees/zccache/releases/latest/download/install.sh | sh
```

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/zackees/zccache/releases/latest/download/install.ps1 | iex"
```

This installs the standalone **native Rust binaries** (`zccache`, `zccache-daemon`,
and `zccache-fp`) directly from GitHub Releases.

Default install locations:

- Linux/macOS user install: `~/.local/bin`
- Linux/macOS global install: `/usr/local/bin`
- Windows user install: `%USERPROFILE%\.local\bin`
- Windows global install: `%ProgramFiles%\zccache\bin`

Global install examples:

```bash
curl -LsSf https://github.com/zackees/zccache/releases/latest/download/install.sh | sudo sh -s -- --global
```

```powershell
powershell -ExecutionPolicy Bypass -c "$env:ZCCACHE_INSTALL_MODE='global'; irm https://github.com/zackees/zccache/releases/latest/download/install.ps1 | iex"
```

Each GitHub release also publishes standalone per-platform archives:

- Linux: `zccache-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz`, `zccache-vX.Y.Z-aarch64-unknown-linux-musl.tar.gz`
- macOS: `zccache-vX.Y.Z-x86_64-apple-darwin.tar.gz`, `zccache-vX.Y.Z-aarch64-apple-darwin.tar.gz`
- Windows: `zccache-vX.Y.Z-x86_64-pc-windows-msvc.zip`, `zccache-vX.Y.Z-aarch64-pc-windows-msvc.zip`

PyPI remains available if you prefer `pip install zccache`; those wheels also install
the native binaries directly onto your PATH. Pre-built wheels are available for:

| Platform | Architecture |
|----------|-------------|
| Linux | x86_64, aarch64 |
| macOS | x86_64, Apple Silicon |
| Windows | x86_64 |

Verify the install:

```bash
zccache --version
```

Rust crates are also published on crates.io. The main installable/runtime crates are:

- `zccache-cli`
- `zccache-daemon`
- `zccache-core`
- `zccache-hash`
- `zccache-protocol`
- `zccache-fscache`
- `zccache-artifact`

Use it as a drop-in replacement for sccache — just substitute `zccache`:

### Integration Summary

```bash
RUSTC_WRAPPER=zccache cargo build
export CC="zccache clang"
export CXX="zccache clang++"
```

- Rust: set `RUSTC_WRAPPER=zccache` or add `rustc-wrapper = "zccache"` to `.cargo/config.toml`.
- Bash: export `RUSTC_WRAPPER`, `CC`, and `CXX` once in your shell or CI environment.
- Python: pass `RUSTC_WRAPPER`, `CC`, and `CXX` through `subprocess` env when invoking `cargo` or `clang`.
- First commands to check: `zccache --version`, `zccache start`, `zccache status`.

<details>
<summary>Rust zccache integration</summary>

Use `zccache` as Cargo's compiler wrapper:

```bash
# one-off invocation
RUSTC_WRAPPER=zccache cargo build
RUSTC_WRAPPER=zccache cargo check

# optional: start the daemon explicitly
zccache start
```

Add to `.cargo/config.toml` for automatic use:

```toml
[build]
rustc-wrapper = "zccache"
```

Recommended project-local config:

```toml
[build]
rustc-wrapper = "zccache"

[env]
ZCCACHE_CACHE_DIR = { value = "/tmp/.zccache", force = false }
```

Supports `--emit=metadata` (`cargo check`), `--emit=dep-info,metadata,link` (`cargo build`),
extern crate content hashing, and cacheable crate types such as `lib`, `rlib`,
and `staticlib`. Proc-macro and binary crates are passed through without caching,
matching the usual `sccache` behavior.

Useful Rust workflow commands:

```bash
# inspect status
zccache status

# clear local cache
zccache clear

# validate wrapper is active
RUSTC_WRAPPER=zccache cargo clean
RUSTC_WRAPPER=zccache cargo check
zccache status
```

</details>

### Cache root override

Set `ZCCACHE_CACHE_DIR` to isolate every zccache cache and state path under a
specific root:

```bash
export ZCCACHE_CACHE_DIR="$HOME/.soldr/cache/zccache"
zccache start
zccache status
```

When set and non-empty, the override is used for `artifacts/`, `tmp/`,
`depgraph/`, `index.redb`, `crashes/`, `logs/`, daemon lock files, download
daemon state, and the default daemon endpoint. Separate cache roots therefore
use separate daemon instances unless `ZCCACHE_ENDPOINT` is explicitly set.
Relative override paths are normalized against the current working directory.

### Daemon namespace override

Set `ZCCACHE_DAEMON_NAMESPACE` to run a second zccache daemon identity against
the same user account or cache root without sharing the default socket, named
pipe, lock file, or lifecycle log:

```bash
export ZCCACHE_DAEMON_NAMESPACE=soldr-dev
zccache start
zccache status --json
```

The unset/default namespace keeps the historical endpoint and path names.
Non-empty namespace values are sanitized to a path-safe ASCII component. For
soldr development, use `ZCCACHE_DAEMON_NAMESPACE=dev` or a more specific value
such as `soldr-dev`. `zccache-daemon-dev` is not a separate shipped binary; it
is represented by the documented namespace mode so the `zccache` CLI, wrapper
mode, and direct daemon entrypoint all resolve the same daemon identity.

### Daemon wire migration

The active daemon wire remains v15 bincode while the v16 prost schema and
dispatcher foundation land. `ZCCACHE_DAEMON_WIRE=prost` is reserved for the
future prost default, and `ZCCACHE_DAEMON_WIRE=bincode` is the documented
fallback spelling for keeping v15 behavior during the migration.

### Worktree cache sharing

#### Safe filesystem materialization

Cache-hit delivery is capability driven. zccache probes the actual source and
target volume pair once and caches the result: reflink (true copy-on-write) is
preferred, otherwise zccache uses registered read-only hardlinks with
copy-before-write and watcher-assisted verification, and finally a plain copy
on cross-volume or limited filesystems. Correctness is identical in every tier;
only disk sharing changes.

Set `ZCCACHE_DISABLE_REFLINK=1` to diagnose or bypass block cloning. Read-only
hardlink enforcement defaults on; set `ZCCACHE_COW_READONLY=0` only as a
compatibility escape hatch. Cache-file mtimes are preserved for reflinks and
hardlinks and are never stamped with the current time. On Windows, placing both
the cache and build target on a ReFS Dev Drive provides the strongest true-COW
tier; prefer a real partition-backed Dev Drive over a VHDX for daily use.

zccache can share cache entries across sibling Git worktrees when the compile is
equivalent. This targets multi-agent workflows where several checkouts of the
same repository build the same Rust crates under different absolute paths. The
daemon detects the enclosing Git root for each compile request, normalizes
project-local source, dependency, cwd, and safe path arguments relative to that
root, and writes cache hits back to the output paths requested by the current
worktree.

For C/C++ projects, enable compiler path remapping so path-sensitive outputs
such as `__FILE__`, debug/source paths, and compatible link search paths can
share cache entries across equivalent Git roots:

```bash
export ZCCACHE_PATH_REMAP=auto
```

In auto mode, zccache discovers the enclosing Git root and internally adds
root/cwd `-ffile-prefix-map=...=.` arguments for GCC/Clang-family compile
misses. The original build files do not need to inject those flags themselves.
For link requests, zccache normalizes proven workspace-local input/search paths
for cache identity while preserving physical output and runtime-facing paths.

Set `ZCCACHE_WORKTREE_ROOT` when automatic Git-root detection is not reliable
or when a wrapper/test needs to define the normalization root explicitly:

```bash
export ZCCACHE_WORKTREE_ROOT="$PWD"
RUSTC_WRAPPER=zccache cargo build
```

For Rust projects, use the same path-remap directive:

```bash
export ZCCACHE_PATH_REMAP=auto
RUSTC_WRAPPER=zccache cargo build
```

In auto mode, zccache discovers the Git worktree root automatically on macOS,
Linux, and Windows, then adds a root-covering rustc `--remap-path-prefix=...=.`
when needed. Set `ZCCACHE_WORKTREE_ROOT` only as an advanced override for
non-Git checkouts or unusual build layouts where automatic root detection is
not reliable.

`ZCCACHE_PATH_REMAP=auto` tells zccache to apply compiler-specific path remaps
when it can prove they are safe, such as C/C++ `-ffile-prefix-map`, Rust
`--remap-path-prefix`, and platform equivalents. The goal is to make emitted
source paths, debug paths, and macro paths stable across equivalent worktrees
without requiring every build generator to spell those flags correctly. Physical
paths that build tools need for dependency tracking, such as Ninja depfiles,
must remain usable for the current checkout.

The override should point at the logical project root shared by equivalent
worktrees. Paths under that root may be normalized for cache identity. Paths
outside that root remain absolute unless zccache has a specific safe rule for
them, so toolchain files, sysroots, generated files outside the checkout, and
other external inputs do not accidentally become shared.

Worktree sharing is intentionally conservative. If zccache cannot prove that a
compile is root-equivalent, it falls back to the existing path-specific cache key
or records a miss. Diagnostics and session logs distinguish normal same-root
hits from worktree-equivalent hits and report conservative reasons such as:

- `git_root_unavailable` - no Git root and no explicit `ZCCACHE_WORKTREE_ROOT`.
- `path_outside_root` - an input path is outside the detected/overridden root.
- `path_sensitive_arg` - flags such as `--remap-path-prefix`, debug path flags,
  or unknown absolute path-bearing options could affect emitted output.
- `content_hash_mismatch` - root-relative paths match but file contents differ.
- `toolchain_mismatch` - the compiler or relevant toolchain inputs differ.
- `unsupported_language` - the invocation is not covered by the worktree-aware
  normalization rules.

The supported worktree-equivalent paths are Rust `rustc` compilation, including
dependency artifacts used through `--extern`, and C/C++ compilation through the
existing depgraph context/artifact keys. The request-level fast path only serves
cross-root hits after validating the current worktree's recorded input hashes;
otherwise zccache falls back to the normal depgraph check or a path-specific
miss.

#### Sub-agent / parallel-worktree recipe

The typical multi-agent workflow runs one sub-agent per `git worktree`, all
checked out from the same repository under sibling paths. Without remap, every
worktree has different absolute compile inputs, so each agent pays full compile
cost even when source contents are identical. With `ZCCACHE_PATH_REMAP=auto`
exported once at the orchestrator level, every sub-agent's compile in every
worktree shares the same logical cache.

1. Create the worktrees. Anything `git worktree add` produces (or a sibling
   `git clone`) works — zccache auto-detects each enclosing Git root:

   ```bash
   git worktree add ../agent-a -b agent-a main
   git worktree add ../agent-b -b agent-b main
   git worktree add ../agent-c -b agent-c main
   ```

2. Export the remap directive once, before launching the agents. Every
   sub-process inherits it; no per-worktree configuration is required:

   ```bash
   export ZCCACHE_PATH_REMAP=auto
   ```

   Then wire zccache into the build the same way you would for a single
   checkout. For Rust:

   ```bash
   export RUSTC_WRAPPER=zccache
   ```

   For C/C++, use the launcher pattern your build system already supports
   (Make and Ninja pick `CC`/`CXX` up automatically):

   ```bash
   # Make / Ninja / plain shell
   export CC="zccache clang"
   export CXX="zccache clang++"
   ```

   ```cmake
   # CMake — set once, applies to every target
   set(CMAKE_C_COMPILER_LAUNCHER zccache)
   set(CMAKE_CXX_COMPILER_LAUNCHER zccache)
   ```

   For Emscripten, swap in `emcc` / `em++`:

   ```bash
   export CC="zccache emcc"
   export CXX="zccache em++"
   ```

   The `ZCCACHE_PATH_REMAP=auto` export is what unlocks cross-worktree
   sharing for whichever language the agent compiles; the wrapper choice is
   just the normal single-checkout setup.

3. Launch sub-agents in their own worktrees in parallel. The first agent to
   compile a unit populates the cache; the others get worktree-equivalent
   hits even though their absolute paths differ:

   ```bash
   (cd ../agent-a && agent-runner ...) &
   (cd ../agent-b && agent-runner ...) &
   (cd ../agent-c && agent-runner ...) &
   wait
   ```

4. Verify it is working. `zccache status` reports worktree-equivalent hits
   separately from same-root hits, and per-session logs include the gate
   reason if a request fell back (`path_outside_root`,
   `content_hash_mismatch`, `toolchain_mismatch`, etc. — see the list above).

A few things worth knowing:

- One daemon, one cache. All worktrees share the same zccache daemon and
  artifact store by default — do not set `ZCCACHE_CACHE_DIR` per worktree, or
  you defeat the sharing.
- Auto-detection requires a Git checkout. The daemon walks ancestors of the
  compile cwd looking for `.git` (file or directory), so plain `git clone`
  and `git worktree add` checkouts both work, but raw source trees
  (tarball extracts, archive payloads, custom build layouts with no `.git`)
  do not. For those, set `ZCCACHE_WORKTREE_ROOT="$PWD"` (or any absolute
  path) to the logical project root you want cache keys normalized against.
  Without either a detected Git root or an explicit override,
  `ZCCACHE_PATH_REMAP=auto` is a no-op and the session log reports
  `git_root_unavailable`.
- User-supplied remap flags take precedence. If your build already passes
  `-ffile-prefix-map=<root>=...` (C/C++/Emscripten) or
  `--remap-path-prefix=<root>=...` (Rust) where `<root>` is the auto-detected
  worktree root, zccache uses your flag as-is and does not inject a
  duplicate. The check is per-flag and per-path: only `-ffile-prefix-map` /
  `--remap-path-prefix` matching the worktree root suppress auto-injection;
  related flags like `-fdebug-prefix-map`, `-fmacro-prefix-map`,
  `-fcoverage-prefix-map`, and `-fprofile-prefix-map` do not. If cwd differs
  from the detected root and you have not supplied a matching
  `-ffile-prefix-map=<cwd>=.`, zccache may still inject one for that path.
  Auto-injected remaps are fallback remaps placed before user-supplied remaps,
  so a narrower overlapping user remap remains the later, winning rule.
- Same-content guarantee. Cross-worktree hits validate content hashes for
  every input. If two worktrees have diverged on a file, the second compile
  misses and recompiles — the cache cannot be poisoned across siblings (the
  invariant fixed in #197).
- Measured win. The
  [`perf_cpp_sibling_remap_warm` / `perf_rustc_sibling_remap_warm`](crates/zccache-daemon/tests/perf_bench_test.rs)
  benchmarks (introduced in #238) confirm warm-state hits across sibling
  worktrees run an order of magnitude faster than bare compiles and sccache
  even though sccache cannot share across sibling roots at all.
- Cached diagnostics follow the same toggle. zccache caches the compiler's
  stdout and stderr alongside the object payload and replays them verbatim on
  every hit. With `ZCCACHE_PATH_REMAP=auto`, the original compile sees the
  injected `-ffile-prefix-map` flags, so the cached diagnostics are already
  worktree-neutral and survive being served to a sibling clone cleanly. With
  `ZCCACHE_PATH_REMAP=off` (the default), the compiler emits absolute paths
  into stderr — for example, a warning whose include trace points at
  `C:\Users\me\dev\fastled5\src\...` — and those bytes are what every
  cross-worktree hit replays into the new build. The `.obj` machine code
  itself is still scrubbed by the #474/#489 fixes for the non-`auto` case
  (per-worktree salting for PCH/MSVC; absolute paths in C/C++ debug info
  only land when their flags are missing), so the contamination is cosmetic:
  confusing diagnostics, never wrong code generation. Set
  `ZCCACHE_PATH_REMAP=auto` to clean diagnostics at the source, or run
  `zccache clear` after upgrading past 1.11.9 to drop pre-fix entries that
  were captured before the per-worktree salting (#474, shipped in 1.11.8)
  and request-cache scoping (#489, shipped in 1.11.9) landed — those entries
  are still served as cross-worktree hits and carry their original-clone
  paths in both the diagnostic stream and (for PCH/MSVC) the artifact bytes
  themselves.

### Strict path validation

Use `--strict-paths` or `ZCCACHE_STRICT_PATHS` to fail fast when compiler path
flags are spelled in ways that can confuse `#pragma once` on Windows.

```bash
zccache --strict-paths=absolute clang++ -c src/main.cpp -IC:/work/project/include
ZCCACHE_STRICT_PATHS=consistent ninja
```

Modes:

- `off` disables validation.
- `consistent` allows relative or absolute paths, but rejects mixed `/` and `\`
  separators within one path or across checked path flags in the same
  invocation.
- `absolute` requires checked path flags to be forward-slash absolute paths
  with no `/./` or `/../` components. `ZCCACHE_STRICT_PATHS=1` maps to this
  mode.

Checked flags include `-I`, `-isystem`, `-iquote`, `-idirafter`, `-include`,
`-include-pch`, `-imacros`, `-F`, `-iframework`, `-imsvc`, and MSVC `/I`.
Response-file arguments are checked after expansion by the daemon.

### Compiler child priority

Compiler and linker subprocesses run with `ZCCACHE_COMPILE_PRIORITY=auto` by
default. Auto mode keeps compiler children at normal priority while total CPU
usage is below 95%, then drops them to low priority when the machine is
saturated. Set the variable on the build command or in the daemon environment to
override it:

| Value | Behavior |
|-------|----------|
| `auto` | Default. Use `normal` below 95% CPU utilization and `low` at 95-100% utilization. |
| `low` | Lower compiler priority (`nice +10` on Unix/macOS, `BELOW_NORMAL_PRIORITY_CLASS` on Windows). |
| `normal` | Preserve the inherited process priority for maximum throughput. |
| `idle` | More conservative background mode (`nice +19` on Unix/macOS, `IDLE_PRIORITY_CLASS` on Windows). |
| `high` | Higher-priority mode for real-time benchmarking (`nice -5` on Unix/macOS where permitted, `HIGH_PRIORITY_CLASS` on Windows). |

Unsupported priority changes fail soft with a daemon log message and do not
break compilation. Invalid values warn and fall back to `low`.

<details>
<summary>Bash integration</summary>

For shell-driven builds, export the wrapper once in your session or CI step:

```bash
export RUSTC_WRAPPER=zccache
export CC="zccache clang"
export CXX="zccache clang++"

zccache start
cargo build
ninja
```

If you want this active in interactive shells, add it to `~/.bashrc`:

```bash
export RUSTC_WRAPPER=zccache
export PATH="$HOME/.local/bin:$PATH"
```

For per-build stats in Bash:

```bash
eval "$(zccache session-start --stats)"
cargo build
zccache session-end "$ZCCACHE_SESSION_ID"
```

</details>

<details>
<summary>Python integration</summary>

Python projects can use `zccache` when invoking Rust or C/C++ toolchains through
`subprocess`, build backends, or extension-module builds.

```python
import os
import subprocess

env = os.environ.copy()
env["RUSTC_WRAPPER"] = "zccache"
env["CC"] = "zccache clang"
env["CXX"] = "zccache clang++"

subprocess.run(["cargo", "build", "--release"], check=True, env=env)
```

This is useful for:

- `setuptools-rust`
- `maturin`
- `scikit-build-core`
- custom Python build/test harnesses that shell out to `cargo`, `clang`, or `clang++`

Example with `maturin`:

```bash
RUSTC_WRAPPER=zccache maturin build
```

Example with Python driving `cargo check`:

```python
subprocess.run(["cargo", "check"], check=True, env=env)
```

</details>

## GitHub Actions

zccache provides a composite GitHub Action that replaces **both** [`mozilla-actions/sccache-action`](https://github.com/mozilla-actions/sccache-action) and [`Swatinem/rust-cache`](https://github.com/Swatinem/rust-cache) with a single action.

### Minimal example

```yaml
name: CI
on: [push, pull_request]

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - uses: dtolnay/rust-toolchain@stable
        with:
          toolchain: 1.94.1

      - uses: zackees/zccache@main
        with:
          shared-key: ${{ runner.os }}

      - run: cargo build --release

      - run: cargo test

      # REQUIRED: always clean up at end of job
      - if: always()
        uses: zackees/zccache/action/cleanup@main
```

### Multi-platform matrix

```yaml
name: CI
on: [push, pull_request]

jobs:
  build:
    strategy:
      fail-fast: false
      matrix:
        include:
          - { os: ubuntu-24.04,     target: x86_64-unknown-linux-gnu }
          - { os: ubuntu-24.04-arm, target: aarch64-unknown-linux-gnu }
          - { os: macos-15,         target: aarch64-apple-darwin }
          - { os: macos-14,         target: x86_64-apple-darwin }
          - { os: windows-2025,     target: x86_64-pc-windows-msvc }
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4

      - uses: dtolnay/rust-toolchain@stable
        with:
          toolchain: 1.94.1
          targets: ${{ matrix.target }}

      # One action replaces sccache + rust-cache
      - uses: zackees/zccache@main
        with:
          shared-key: ${{ matrix.target }}

      - run: cargo build --release --target ${{ matrix.target }}
      - run: cargo test --target ${{ matrix.target }}

      - if: always()
        uses: zackees/zccache/action/cleanup@main
```

### What it does

The action provides two default cache layers, plus an opt-in target snapshot layer
and `zccache warm` for near-instant subsequent builds:

| Layer | What | Replaces | Effect |
|---|---|---|---|
| **Compilation cache** | Per-unit `.o`/`.rlib` files via zccache daemon | sccache | ~1ms per cache hit vs ~170ms for sccache |
| **Cargo registry cache** | `~/.cargo/registry/` + `~/.cargo/git/` | Swatinem/rust-cache | Avoids re-downloading crates |
| **Target snapshot cache** | `target/` tarball excluding `incremental/` | (new) | Cargo sees target outputs and fingerprints together |
| **`zccache warm`** | Backfills `target/deps/` from compilation cache | (new) | Restores missing artifacts before cargo runs |

On setup, the action installs zccache, then restores caches through the native
`zccache gha-cache` backend when the GitHub Actions cache runtime is available.
When that runtime is missing, it falls back to `actions/cache`. When
`cache-target: true` is set, it also extracts the target snapshot, runs
`zccache warm` to backfill cached `.rlib`/`.rmeta` files, and touches all
timestamps to a single consistent value.

On cleanup: stops daemon and saves the enabled caches. The native backend saves
the compilation cache, the cargo registry archive, and the optional target
snapshot without requiring manual cache steps in the workflow. When prefix
restore-fallback is enabled, the action still uses `actions/cache` for that
fallback path. Target snapshots are pruned and size-checked before save.

### CI benchmark results

Measured on `ubuntu-24.04` building `zccache-core` (14 crates):

| Scenario | Bare | sccache | zccache |
|---|---|---|---|
| 1st CI run (clean target) | 5,315ms | 3,261ms | **2,194ms** |
| **2nd CI run (cached target)** | 5,315ms | 3,261ms | **~200ms** |

**15x faster than sccache on subsequent CI runs.** Zero recompilation — cargo sees all fingerprints as fresh and prints `Finished` immediately.

How it works:
1. First run with `cache-target: true`: cold build, populates zccache compilation cache and saves a bounded target snapshot.
2. Second run with `cache-target: true`: restores the target snapshot, runs `zccache warm` as a backfill, touches timestamps, then `cargo build` finishes without recompilation.

`zccache warm` reads the on-disk artifact index (no daemon needed) and filters by `Cargo.lock` — only restores artifacts matching crates in your lockfile. That is a speed optimization, not a full integrity-verification pass: warmed artifacts are trusted and Cargo is expected to reject or rebuild anything incompatible.

### Inputs

| Input | Default | Description |
|---|---|---|
| `cache-cargo-registry` | `true` | Cache cargo registry index + crate files + git deps |
| `cache-compilation` | `true` | Cache compilation units via zccache daemon |
| `cache-target` | `false` | Cache target snapshot + run `zccache warm`; opt in only for workflows where target snapshots are worth the disk budget |
| `target-snapshot-mode` | `hot` | `hot` saves Cargo metadata plus target files read or modified during the job; `full` saves the pruned target tree |
| `target-snapshot-max-size` | `2GiB` | Skip or fail target snapshot save when the pruned snapshot exceeds this size; use `0` for unlimited |
| `target-snapshot-too-large` | `skip` | `skip` oversized target snapshots or `fail` cleanup |
| `target-prune-incremental` | `true` | Remove `target/**/incremental` before creating a snapshot |
| `target-prune-build-script-out` | `false` | Remove `target/**/build/*/out` before creating a snapshot |
| `compilation-restore-fallback` | `true` | Allow prefix fallback for compilation cache restores |
| `target-restore-fallback` | `false` | Allow prefix fallback for target snapshot restores |
| `target-dir` | `target` | Path to the cargo target directory |
| `shared-key` | `""` | Extra key for matrix isolation (typically the target triple) |
| `zccache-version` | `latest` | Version to install |
| `save-cache` | `true` | Set `false` for PR builds (restore-only, saves cache budget) |

### Restore policy

The action now treats the two cache layers differently:

- Compilation cache fallback stays enabled by default. That preserves fast incremental reuse across nearby commits while still letting zccache validate cache hits when `rustc` actually runs.
- Target snapshot fallback is disabled by default. Reusing stale Cargo fingerprints and build-script outputs across different source trees can make a PR merge ref look fresh when it is not.
- Target snapshots are disabled by default because Cargo does not garbage collect `target/`. When enabled, the default `target-snapshot-mode: hot` saves Cargo freshness metadata plus target files read or modified during the job instead of archiving the whole tree. Use `target-snapshot-mode: full` only for tightly scoped jobs where the target directory is known to stay bounded.
- Target snapshot saves prune `target/**/incremental` by default, can optionally prune `target/**/build/*/out`, and skip saving when the pruned snapshot exceeds `target-snapshot-max-size`.

Target snapshots are legacy action-only behavior for `cache-target: true`
workflows. soldr/setup-soldr integrations should use `zccache rust-plan` for
target artifact restore/save behavior; see
`docs/architecture/target-cache.md` for the ownership boundary.

If you want the old fastest-possible behavior for developer CI, opt back in explicitly:

```yaml
- uses: zackees/zccache@main
  with:
    cache-target: true
    compilation-restore-fallback: true
    target-restore-fallback: true
```

If you want a more release-hardened setup, keep target snapshots disabled and prefer exact restores:

```yaml
- uses: zackees/zccache@main
  with:
    compilation-restore-fallback: false
```

This project is optimized for developer speed, not full artifact attestation. `zccache warm` does not checksum every restored object on every run, and the action does not try to prove cache integrity before building. If you need that level of assurance, disable the speed-focused layers for that workflow.

### Outputs

| Output | Description |
|---|---|
| `cache-hit-compilation` | Whether the zccache compilation cache was restored |
| `cache-hit-registry` | Whether the cargo registry cache was restored |
| `cache-hit-target` | Whether the target snapshot cache was restored |

### Why two parts?

Composite GitHub Actions don't support `post` steps (automatic cleanup). The action is split into:

1. **`zackees/zccache`** — setup: install zccache, restore caches through the native GHA cache backend when available, optionally warm target, start daemon, set `RUSTC_WRAPPER`
2. **`zackees/zccache/action/cleanup`** — teardown: print stats, stop daemon, prune and save enabled caches through the same backend

The cleanup action **must** be called with `if: always()` to ensure caches are saved even on failure.

### Migrating from sccache + rust-cache

Before (two actions):
```yaml
- uses: mozilla-actions/sccache-action@v0.0.9
- uses: Swatinem/rust-cache@v2
env:
  SCCACHE_GHA_ENABLED: "true"
  RUSTC_WRAPPER: sccache
```

After (one action):
```yaml
- uses: zackees/zccache@main
  with:
    shared-key: ${{ matrix.target }}
# ... build steps ...
- if: always()
  uses: zackees/zccache/action/cleanup@main
```

No env vars needed — the action sets `RUSTC_WRAPPER=zccache` automatically.

---

### C/C++ build system integration (ninja, meson, cmake, make)

zccache is a **drop-in compiler wrapper**. Point your build system's compiler
at `zccache <real-compiler>` and it handles the rest:

```ini
# meson native file
[binaries]
c = ['zccache', '/usr/bin/clang']
cpp = ['zccache', '/usr/bin/clang++']
```

```cmake
# CMake
set(CMAKE_C_COMPILER_LAUNCHER zccache)
set(CMAKE_CXX_COMPILER_LAUNCHER zccache)
```

The first build (cold cache) runs at near-bare speed. Subsequent rebuilds
(`ninja -t clean && ninja`, or touching source files) serve cached artifacts
via hardlinks in under a second.

**Strict path validation:** Set `ZCCACHE_STRICT_PATHS` or pass
`--strict-paths=<off|consistent|absolute>` before the compiler name to catch
non-normalized include paths before the real compiler runs:

```bash
ZCCACHE_STRICT_PATHS=consistent ninja
zccache --strict-paths=absolute clang++ -IC:/project/src -c main.cpp
```

`consistent` rejects checked path flags that mix separator styles within one
path or across the same invocation. `absolute` also requires path flags such as
`-I`, `-isystem`, `-include`, and `-include-pch` to be forward-slash absolute
paths without `/./` or `/../` segments. Violations exit non-zero with the
offending flag and full caller command.

**Path remap auto mode:** Planned C/C++ worktree sharing uses
`ZCCACHE_PATH_REMAP=auto` to let zccache inject and key compiler path remaps
internally for clang/gcc/emcc builds. This keeps Ninja, Meson, CMake, and Make
commands simple while allowing sibling checkouts to share artifacts when
compiler-visible source/debug paths are equivalent.

**Single-roundtrip IPC:** In drop-in mode, zccache sends a single
`CompileEphemeral` message that combines session creation, compilation, and
session teardown — eliminating 2 of 3 IPC roundtrips per invocation.

**Session stats:** Track hit rates per-build with `--stats`:

```bash
eval $(zccache session-start --stats --log build.log)
export ZCCACHE_SESSION_ID=...
# ... build runs ...
zccache session-stats $ZCCACHE_SESSION_ID   # query mid-build
zccache session-end $ZCCACHE_SESSION_ID     # final stats
```

Render the always-on engine phase profiler from a saved stats JSON file:

```bash
zccache session-end $ZCCACHE_SESSION_ID --json > last-session-stats.json
zccache engine-profile last-session-stats.json
zccache engine-profile last-session-stats.json --json
```

This reports aggregate hit/miss phase totals, averages, and dominant phases
from `phase_profile`. It is the cache-engine regression view; Tokio Console is
for live async runtime symptoms such as blocked tasks, long polls, and resource
contention.

**Persistent cache:** Artifacts are stored in `~/.zccache/artifacts/`
and survive daemon restarts. No need to re-warm the cache after a reboot.

**Compile journal (build replay):** Every compile and link command is recorded
to `~/.zccache/logs/compile_journal.jsonl` as a JSONL file with enough
detail to replay the entire build:

```json
{"ts":"2026-03-17T10:30:00.123Z","outcome":"hit","compiler":"/usr/bin/clang++","args":["-c","foo.cpp","-o","foo.o"],"cwd":"/project/build","env":[["CC","clang"]],"exit_code":0,"session_id":"uuid","latency_ns":1234567}
```

Fields: `ts` (ISO 8601 UTC), `outcome` (`hit`/`miss`/`error`/`link_hit`/`link_miss`),
`compiler` (full path), `args` (full argument list), `cwd`, `env` (omitted when
inheriting daemon env), `exit_code`, `session_id` (null for ephemeral),
`latency_ns` (wall-clock nanoseconds). One JSON object per line — pipe through
`jq` to filter, or replay builds by extracting compiler + args + cwd.

**Per-session compile journal:** Pass `--journal <path>` to `session-start` to
write a dedicated JSONL log containing only the commands from that session.
The path must end in `.jsonl`:

```bash
result=$(zccache session-start --journal build.jsonl)
session_id=$(echo "$result" | jq -r .session_id)
export ZCCACHE_SESSION_ID=$session_id

# ... build runs ...

# Inspect this session's commands only (no noise from other sessions)
jq . build.jsonl

zccache session-end $session_id
```

The session journal uses the same JSONL schema as the global journal. Entries
are written to both the global and session journals simultaneously. The session
file handle is released when `session-end` is called.

### Multi-file compilation (fast path)

When a build system passes multiple source files to a single compiler invocation
(e.g. `gcc -c a.cpp b.cpp c.cpp -o ...`), zccache treats this as a **fast path**:

1. Each source file is checked against the cache **in parallel**.
2. Cache hits are served immediately — their `.o` files are written from the cache.
3. Remaining cache misses are batched into a **single compiler process**, preserving
   the compiler's own process-reuse and memory-sharing benefits.
4. The outputs of the batched compilation are cached individually for future hits.

This hybrid approach means the first build populates the cache per-file, and
subsequent builds serve as many files as possible from cache while still letting
the compiler handle misses efficiently in bulk.

**Recommendation:** Configure your build system to pass multiple source files per
compiler invocation whenever possible. This gives zccache the best opportunity
to parallelize cache lookups and minimize compiler launches.

### Concurrency

The daemon uses lock-free concurrent data structures (DashMap) for artifact and
metadata lookups, so parallel compilation requests from multiple build workers
never serialize on a global lock.

## Status

**Early development** — architecture and scaffolding phase.

## Goals

- Extremely fast on local machines (daemon keeps caches warm)
- Portable across Linux, macOS, and Windows
- Correct under heavy parallel compilation (no stale cache hits)
- Simple deployment (single binary)

## Tool Compatibility

zccache works as a drop-in wrapper for these compilers and tools:

- **Clang Toolchain:** [clang](https://clang.llvm.org/), [clang-tidy](https://clang.llvm.org/extra/clang-tidy/), [IWYU](https://include-what-you-use.org/)
- **Emscripten / WebAssembly:** [emcc](https://emscripten.org/), [wasm-ld](https://lld.llvm.org/WebAssembly.html)
- **Rust Toolchain:** [rustc](https://www.rust-lang.org/), [rustfmt](https://github.com/rust-lang/rustfmt), [clippy](https://github.com/rust-lang/rust-clippy)

## Architecture

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full system design.

### Key components

| Crate | Purpose |
|-------|---------|
| `zccache-cli` | Command-line interface (`zccache` binary) — includes `warm`, `cargo-registry`, `gha-cache` subcommands |
| `zccache-daemon` | Daemon process (IPC server, orchestration) |
| `zccache-core` | Shared types, errors, config, path utilities |
| `zccache-protocol` | IPC message types and serialization |
| `zccache-ipc` | Transport layer (Unix sockets / named pipes) |
| `zccache-hash` | blake3 hashing and cache key computation |
| `zccache-fscache` | In-memory file metadata cache |
| `zccache-artifact` | Disk-backed artifact store with redb index |
| `zccache-watcher` | File watcher subsystem: daemon `notify` pipeline plus Rust-backed Python watcher bindings |
| `zccache-compiler` | Compiler detection and argument parsing |
| `zccache-gha` | GitHub Actions Cache API client |
| `zccache-test-support` | Test utilities and fixtures |

## Building

```bash
cargo build --workspace
```

## Testing

```bash
cargo test --workspace
```

## Documentation

- [Architecture](docs/ARCHITECTURE.md)
- [Design Decisions](docs/DESIGN_DECISIONS.md)
- [Roadmap](docs/ROADMAP.md)

## Watcher APIs

zccache exposes watcher-related APIs in three different places, depending on
how you want to consume change detection:

- CLI: `zccache fp ...` for daemon-backed fingerprint checks in scripts and CI
- Python: `zccache.watcher` for cross-platform library-style file watching
- Rust: `zccache-watcher` for the daemon-facing watcher pipeline primitives

### CLI API

The CLI watcher entrypoint is the fingerprint API. It answers "should I rerun?"
by consulting the daemon's in-memory watch state and cached file fingerprints.

```bash
zccache fp --cache-file .cache/headers.json check \
  --root . \
  --include '**/*.cpp' \
  --include '**/*.h' \
  --exclude build \
  --exclude .git
```

Exit codes:

- `0`: files changed, run the expensive step
- `1`: no changes detected, skip the step

After a successful or failed run, update the daemon's watch state:

```bash
zccache fp --cache-file .cache/headers.json mark-success
zccache fp --cache-file .cache/headers.json mark-failure
zccache fp --cache-file .cache/headers.json invalidate
```

The fingerprint API is the best fit for shell scripts, CI jobs, and build
steps that only need a yes/no change answer rather than a stream of file events.

### Python API

`pip install zccache` now exposes an importable `zccache` module in addition to
the native binaries. The Python surface is aimed at the same hot-path features
the CLI already exposes: watcher events, fingerprint decisions, daemon/session
control, downloads, and Arduino `.ino` conversion.

```python
from zccache.client import ZcCacheClient
from zccache.fingerprint import FingerprintCache
from zccache.ino import convert_ino
from zccache.watcher import watch_files

client = ZcCacheClient()
client.start()

fp = FingerprintCache(".cache/watch.json")
decision = fp.check(
    root=".",
    include=["**/*.cpp", "**/*.hpp", "**/*.ino"],
    exclude=["**/.build/**", "**/fastled_js/**"],
)
if decision.should_run:
    convert_ino("Blink.ino", "build/Blink.ino.cpp")
    fp.mark_success()
```

The watcher API remains polling- and callback-friendly, while the backend runs
the filesystem scan loop in Rust and only crosses into Python when delivering
events.

```python
from zccache.watcher import watch_files

watcher = watch_files(
    ".",
    include_folders=["src", "include"],
    include_globs=["src/**/*.cpp", "include/**/*.h"],
    exclude_globs=["build", "dist/**", ".git"],
    debounce_seconds=0.2,
    poll_interval=0.1,
)

event = watcher.poll(timeout=1.0)
if event is not None:
    print(event.paths)

watcher.stop()
```

For explicit lifecycle control, use the class API:

```python
from zccache.watcher import FileWatcher

watcher = FileWatcher(".", include_globs=["**/*.cpp"], autostart=False)
watcher.start()
event = watcher.poll(timeout=1.0)
watcher.stop()
watcher.resume()
watcher.stop()
```

Python watcher features:

- `include_folders` to narrow the scan roots
- `include_globs` to include only matching files
- `exclude_globs` / `excluded_patterns` to skip directories or files
- `debounce_seconds` to coalesce bursts of edits
- optional `notification_predicate` applied at Python delivery time
- callback API plus polling API
- explicit `start()`, `stop()`, `resume()`, and context-manager support

Daemon/session control is also available without shelling out per call:

```python
from zccache.client import ZcCacheClient

client = ZcCacheClient()
client.start()
session = client.session_start(cwd=".", track_stats=True)
stats = client.session_stats(session.session_id)
client.session_end(session.session_id)
```

And fingerprint state can be managed directly from Python:

```python
from zccache.fingerprint import FingerprintCache

fp = FingerprintCache(".cache/lint.json", cache_type="two-layer")
decision = fp.check(root=".", include=["**/*.cpp"], exclude=["**/.build/**"])
if decision.should_run:
    fp.mark_success()
```

Compatibility wrappers used by `fastled-wasm` are also available:

- `FileWatcherProcess`
- `DebouncedFileWatcherProcess`
- `watch_files`
- `FileWatcher`

See [crates/zccache-watcher/README.md](crates/zccache-watcher/README.md) for the
full Python watcher surface.

### Rust API

For Rust consumers, the public watcher crate is [`zccache-watcher`](crates/zccache-watcher/README.md).
It now exposes both the daemon-facing watcher pipeline and a library-style
polling watcher API:

- `PollingWatcherConfig`
- `PollingWatcher`
- `PollWatchBatch`
- `PollWatchObserver`

- `IgnoreFilter` for directory-name-based filtering
- `NotifyWatcher` for `notify`-backed OS watch registration
- `SettleBuffer` and `SettledEvent` for burst coalescing
- `OverflowRecovery` for overflow-driven rescan scheduling
- `WatchEvent` and `WatcherConfig` for event/config plumbing

Example:

```rust
use std::time::Duration;
use zccache_watcher::{PollingWatcher, PollingWatcherConfig};

let mut config = PollingWatcherConfig::new(".");
config.include_globs = vec!["**/*.cpp".to_string()];
config.poll_interval = Duration::from_millis(50);
config.debounce = Duration::from_millis(50);

let watcher = PollingWatcher::new(config)?;
watcher.start()?;
let batch = watcher.poll_timeout(Duration::from_secs(1))?;
watcher.stop()?;
```

## Downloader APIs

zccache also exposes the dedicated download subsystem in three places:

- CLI: `zccache download ...` on the main binary, plus the standalone `zccache-download` tool
- Python: `zccache.downloader.DownloadApi`
- Rust: `zccache-download-client` for the client API and `zccache-download` for shared download types

The downloader daemon is separate from the compiler-cache daemon. It is meant for
long-lived artifact downloads, deterministic cache paths, optional unarchiving,
and attach/wait/status flows from multiple clients.

### Downloader CLI

The main `zccache` binary includes a simple download subcommand:

```bash
zccache download \
  https://example.com/toolchain.tar.zst \
  --unarchive .cache/toolchain \
  --sha256 0123456789abcdef \
  --multipart-parts 8
```

That path blocks until the artifact is ready and prints the resolved cache path,
SHA-256, and optional unarchive destination.

For daemon lifecycle control, attach/wait/status operations, JSON output, and
explicit archive-format selection, use the standalone downloader CLI:

```bash
zccache-download daemon start

zccache-download fetch \
  https://example.com/toolchain.tar.zst \
  .cache/downloads/toolchain.tar.zst \
  --expanded .cache/toolchain \
  --archive-format tar.zst \
  --max-connections 8

zccache-download exists \
  https://example.com/toolchain.tar.zst \
  .cache/downloads/toolchain.tar.zst

zccache-download --json daemon status
```

Additional standalone subcommands:

- `get` to attach to a raw download handle
- `wait`, `status`, and `cancel` for handle lifecycle operations
- `daemon stop` to shut the download daemon down explicitly

### Python Downloader API

`pip install zccache` exposes the downloader as `zccache.downloader`.

```python
from zccache.downloader import DownloadApi

api = DownloadApi()
api.start()

result = api.download(
    source_url="https://example.com/toolchain.tar.zst",
    destination=".cache/downloads/toolchain.tar.zst",
    expanded=".cache/toolchain",
    archive_format="tar.zst",
    multipart_parts=8,
)
print(result.status, result.sha256, result.expanded_path)

state = api.exists(
    source_url="https://example.com/toolchain.tar.zst",
    destination=".cache/downloads/toolchain.tar.zst",
)
print(state.kind, state.reason)
```

If you need attach/wait/status semantics instead of a blocking fetch call, use
`DownloadApi.attach(...)` and operate on the returned `DownloadHandle`:

```python
from zccache.downloader import DownloadApi

api = DownloadApi()
with api.attach(
    source_url="https://example.com/toolchain.tar.zst",
    destination=".cache/downloads/toolchain.tar.zst",
    max_connections=8,
) as handle:
    status = handle.wait(timeout_ms=1_000)
    print(handle.download_id, status.phase, status.downloaded_bytes)
```

The Python downloader surface includes:

- `DownloadApi.start()`, `stop()`, and `daemon_status()`
- `DownloadApi.download()` / `fetch()` for blocking or non-blocking fetches
- `DownloadApi.exists()` for cache-state checks
- `DownloadApi.attach()` plus `DownloadHandle.status()`, `wait()`, and `cancel()`

### Rust Downloader API

For Rust code, use `zccache-download-client` as the entrypoint and
`zccache-download` for shared status and option types.

```rust
use std::path::PathBuf;
use zccache_download_client::{ArchiveFormat, DownloadClient, FetchRequest, WaitMode};

let client = DownloadClient::new(None);
client.start_daemon()?;

let mut request = FetchRequest::new(
    "https://example.com/toolchain.tar.zst",
    PathBuf::from(".cache/downloads/toolchain.tar.zst"),
);
request.destination_path_expanded = Some(PathBuf::from(".cache/toolchain"));
request.archive_format = ArchiveFormat::TarZst;
request.multipart_parts = Some(8);
request.wait_mode = WaitMode::Block;

let result = client.fetch(request)?;
println!("{:?} {} {}", result.status, result.sha256, result.cache_path.display());
```

For handle-based control, use `DownloadClient::download(...)`:

```rust
use std::path::Path;
use zccache_download::DownloadOptions;
use zccache_download_client::DownloadClient;

let client = DownloadClient::new(None);
let mut handle = client.download(
    "https://example.com/toolchain.tar.zst",
    Path::new(".cache/downloads/toolchain.tar.zst"),
    DownloadOptions {
        force: false,
        max_connections: Some(8),
        min_segment_size: None,
    },
)?;

let status = handle.wait(Some(1_000))?;
println!("{:?} {}", status.phase, status.downloaded_bytes);
```

The Rust downloader surface includes:

- `DownloadClient::start_daemon()`, `stop_daemon()`, and `daemon_status()`
- `DownloadClient::fetch()` and `exists()` with `FetchRequest`
- `DownloadClient::download()` returning a `DownloadHandle`
- `ArchiveFormat`, `FetchResult`, `FetchState`, `FetchStatus`, and `WaitMode`
- `DownloadOptions`, `DownloadStatus`, and `DownloadDaemonStatus`

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

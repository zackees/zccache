# `python/zccache/`

Python package distributed alongside the zccache daemon. Exposes
typed APIs over the daemon's IPC, the filesystem watcher, the
fingerprint cache, and the downloader. All bindings to native code go
through `zccache._native`, a maturin-built pyo3 module (the `.pyd` /
`.so` lives next to each subpackage's `_native` import).

## Layout

```
python/zccache/
├── __init__.py              # public re-exports (see __all__)
├── client.py                # ZcCacheClient + DaemonStatus + SessionStats
├── downloader.py            # DownloadApi + FetchResult + FetchState
├── fingerprint/             # FingerprintCache + FingerprintManager
├── watcher/                 # FileWatcher + DebouncedFileWatcherProcess
├── ino.py                   # InoConvertResult + convert_ino
├── cli.py                   # `zccache-python` console-script entry
└── cpp_lint/                # Streaming C/C++ lint cache (issue #841)
```

## Public surface

Stable, re-exported from `zccache` directly. Every symbol in
`__all__` is part of the package's contract — moving or renaming one
is a breaking change. New surface lands additively.

| Module           | What it exposes                                                  |
|------------------|------------------------------------------------------------------|
| `client`         | `ZcCacheClient`, `DaemonStatus`, `SessionStartResult`, `SessionStats`. |
| `downloader`     | `DownloadApi`, `DownloadHandle`, `FetchResult`, `FetchState`.    |
| `fingerprint`    | `FingerprintCache`, `FingerprintManager`, `FingerprintResult`.   |
| `watcher`        | `FileWatcher`, `DebouncedFileWatcherProcess`, `watch_files`.     |
| `cpp_lint`       | `cpp_lint`, `LintInput`, `AstQuery`, `IwyuItem`, `ResultItem`, `Summary`, ... (see `cpp_lint/README.md`). |

## Layout invariants

- Every subpackage that wraps a Rust binding ships its own
  `_native.{pyd,so,dylib}` alongside its Python source. The dispatch
  shim lives in the subpackage; the `._native` module is the
  pyO3-exported surface.
- `__init__.py` re-exports flat — callers do `from zccache import
  FileWatcher`, not `from zccache.watcher import FileWatcher`. The
  latter still works and is the canonical import for type stubs.
- All public dataclasses are `@dataclass(frozen=True)` so they're
  hashable and safe to share across threads / serialize.

## Building the native module

The `_native.pyd` files are produced by `ci/build_dist.py` (see
[`../../ci/README.md`](../../ci/README.md)) and shipped inside the
distribution wheels. Local dev imports them in-place — they're
gitignored under each `.cpython-XYZ-*.{pyd,so}` pattern.

## `cpp_lint` subpackage

Pure-Python implementation of the streaming C/C++ lint cache from
[issue #841](https://github.com/zackees/zccache/issues/841). See
[`cpp_lint/README.md`](./cpp_lint/README.md) for the per-(TU, item)
caching model, scope/ignore resolution, tool-fetch policy, and abort
semantics. Daemon-side integration (depgraph oracle, pyo3 feeder
thread, JSONL event log hook) lands incrementally — the dataclass
surface in this PR is the long-term contract.

## Tests

Tests live under `python/tests/`. Modules that need the native
extension call `pytest.importorskip("zccache._native")` so a pure-py
test run still passes on a checkout that hasn't built the wheel.
`cpp_lint` tests are stdlib-only by default; integration tests fetch
clang-query via `clang-tool-chain-bins` and skip cleanly if no network.

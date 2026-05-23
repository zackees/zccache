# zccache-fingerprint

Lightweight fingerprint cache for CI and tooling.

Answers "has this set of files changed since the last successful operation?" without the full machinery of the artifact store or metadata cache.

## Cache Types

- **TwoLayerCache** (default) — Per-file mtime+size → blake3 fingerprinting. Skips hashing when mtime is unchanged (Layer 1). When mtime differs but content hasn't, updates the cached mtime silently (Layer 2, smart touch handling).
- **HashCache** — Single aggregate blake3 hash of an entire file set. Suited for all-or-nothing decisions like "run all tests".

Both use the pending pattern (pre-compute → mark success/failure) for crash safety.

## CLI

### Subcommands

| Command | Description | Exit 0 | Exit 1 | Exit 2 |
|---|---|---|---|---|
| `check` | Scan files and check for changes | Run needed | Skip (cache hit) | Error |
| `mark-success` | Record that the operation succeeded | OK | — | Error |
| `mark-failure` | Record that the operation failed | OK | — | Error |
| `invalidate` | Delete the cache (forces re-run) | OK | — | Error |

### Global Options

```
--cache-file <PATH>              Path to the cache file (required)
--cache-type <hash|two-layer>    Cache algorithm (default: two-layer)
```

### Check Options

```
--root <DIR>          Root directory to scan (default: current directory)
--ext <EXT>           File extension filter, without dot (repeatable)
--include <GLOB>      Glob include pattern (repeatable, conflicts with --ext)
--exclude <PATTERN>   Exclude pattern (repeatable)
```

When `--ext` is used, `--exclude` values are directory names (e.g., `target`).
When `--include` is used, `--exclude` values are glob patterns (e.g., `target/**`).

### Usage

```bash
# Check if Rust files changed, run lint if so
if zccache-fingerprint --cache-file .cache/lint.json check --ext rs --exclude target; then
    cargo clippy
    zccache-fingerprint --cache-file .cache/lint.json mark-success
fi

# Glob-based scanning
zccache-fingerprint --cache-file .cache/build.json check \
    --include "src/**/*.rs" --include "Cargo.toml" \
    --exclude "target/**"

# Use hash cache for all-or-nothing decisions
zccache-fingerprint --cache-file .cache/test.json --cache-type hash check --ext rs

# Force re-run by clearing the cache
zccache-fingerprint --cache-file .cache/lint.json invalidate
```

### Workflow

1. `check` scans files and pre-computes fingerprints into a `.pending` file
2. Run your operation (lint, test, build)
3. `mark-success` atomically promotes the pre-computed fingerprint — immune to file changes during step 2
4. Next `check` compares against the saved fingerprint and returns skip if nothing changed

If `mark-failure` is called instead, the next `check` will always return "run needed".

## Python Bindings

Install from PyPI:

```bash
pip install zccache-fingerprint
```

### Quick Start

```python
from zccache.fingerprint import Api, FingerprintResult, FingerprintManager

# Hash a directory (Rust + blake3, fast)
h = Api.hash_files("src", ["rs", "toml"], [".git", "target"])

# Glob-based scanning
h = Api.hash_files_glob(".", ["src/**/*.rs", "Cargo.toml"], ["target/**"])

# Per-file hashes
for path, hash in Api.walk_and_hash("src", ["rs"]):
    print(f"{path}: {hash}")

# Convenience: parse "**/*.h,**/*.cpp" glob strings
h = Api.hash_directory("src", "**/*.h,**/*.cpp,**/*.hpp")

# Full fingerprint with timing
result = Api.fingerprint_code_base("src")
print(result.hash, result.elapsed_seconds)
```

### Cache Management

```python
from pathlib import Path
from zccache.fingerprint import Api, FingerprintResult, FingerprintManager

mgr = FingerprintManager(cache_dir=Path(".cache"), build_mode="debug")

should_run = mgr.check("my_tests", lambda: FingerprintResult(
    hash=Api.hash_files("src", ["rs"], [".git", "target"])
))

if should_run:
    run_tests()
    mgr.update_test_metadata("my_tests", num_tests_run=42, num_tests_passed=42, duration_seconds=1.5)
    mgr.save_all("success")
```

### Building from Source

```bash
cd crates/zccache-fingerprint
uv venv .venv
uv pip install maturin pytest --python .venv/Scripts/python.exe
.venv/Scripts/python.exe -m maturin develop --features python
.venv/Scripts/python.exe -m pytest python/tests/ -v
```

## Rust Library

The crate also exports its API for use as a Rust library:

```rust
use zccache_monocrate::fingerprint::{walk_files, TwoLayerCache, CacheDecision};

let files = walk_files(root, &["rs"], &["target"])?;
let cache = TwoLayerCache::new(cache_path);

match cache.check(&files)? {
    CacheDecision::Skip => println!("nothing changed"),
    CacheDecision::Run(reason) => {
        println!("running: {reason}");
        // ... do work ...
        cache.mark_success()?;
    }
}
```

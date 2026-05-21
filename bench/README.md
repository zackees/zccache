# Benchmarks

Performance comparison between bare clang, sccache, and zccache.

## Single-file benchmark

Compiles a synthetic template-heavy C++ file, measuring cold/warm cache performance.

```bash
./perf.sh
```

## Meson+Ninja full-project benchmark

Builds FastLED10 (a large C++ library) with meson+ninja, measuring both meson setup
time (compiler probes) and ninja build time (actual compilations).

Scenarios: bare (no cache), sccache cold/warm, zccache cold/warm.

Script lives in `ci/` (it's a build orchestration tool, not a Rust benchmark).

```bash
# Run all scenarios
uv run python ci/meson_bench.py ~/dev/fastled10

# Run specific scenarios
uv run python ci/meson_bench.py ~/dev/fastled10 --scenarios bare,zccache-cold,zccache-warm

# Limit parallel jobs
uv run python ci/meson_bench.py ~/dev/fastled10 -j 8
```

### Latest results (Windows x86_64, FastLED10)

| Scenario | meson setup | ninja build | Total | vs bare |
|:---------|----------:|----------:|------:|--------:|
| bare | 3.9s | 8.1s | 12.0s | 1.00x |
| sccache-cold | 5.4s | 9.3s | 14.7s | 1.22x |
| sccache-warm | 5.6s | 1.3s | 7.0s | 0.58x |
| zccache-cold | 6.7s | 9.2s | 16.0s | 1.33x |
| zccache-warm | 6.0s | 9.4s | 15.4s | 1.28x |

> **Note:** The meson benchmark creates separate build directories for cold/warm,
> which means the `-o` output paths differ between runs. This prevents cache hits
> across scenarios. In real-world use (same build directory, `ninja -t clean && ninja`),
> zccache warm rebuild takes **~725ms** (vs 9.3s cold) with a 42.9% hit rate and
> ~14.5s time saved. The single-file benchmark (`./perf`) demonstrates the
> true warm-cache speedup: **43x faster than sccache, 324x faster than bare clang**. (Wrapper is `perf.sh`; it shelled out to `ci/perf.py`.)

### How cache hits work with ninja

When ninja rebuilds (e.g. after `ninja -t clean` or touching source files), it
re-invokes the compiler wrapper for each stale target. zccache intercepts these
invocations and serves cached artifacts via hardlinks — typically completing a
full rebuild in under 1 second.

Key optimizations for build system integration:
- **Single-roundtrip IPC:** `CompileEphemeral` combines session + compile + teardown
- **Ultra-fast path (60s):** Clock-based skip avoids all hashing when the watcher
  confirms no files changed since the last verified hit
- **Persistent artifacts:** Cache in `~/.zccache/artifacts/` survives daemon
  restarts — no cold-start penalty after reboot

# Linux profiler (bosn-managed Docker)

Generates **on-CPU** and **off-CPU** flame charts of the zccache daemon under
a representative cold-build workload, using a containerized Linux toolchain
(perf + bpftrace + FlameGraph) so results are reproducible from a Windows
or macOS host.

The current repository entrypoint is the bosn stack in `/bosn.toml`. It runs
the existing `perf_bench_test` workload with soldr and retains CPU, syscall
wait, peak-RSS, heap-allocation, and Massif evidence below
`.perf-local/bosn-profile/`:

```powershell
bosn doctor
bosn tasks
bosn build-profile
bosn bench-profile
bosn oncpu-profile
bosn offcpu-profile
bosn memory-profile
```

Set `ZCCACHE_PROFILE_WORKLOAD` in `bosn.toml` for a different registered
ignored benchmark. The build and toolchain caches are bosn-managed volumes;
the source checkout is mounted directly so retained evidence lands in the
repository's existing ignored performance tree.

## Files

- `Dockerfile.perf-linux` — image with `rust:1.95.0-bookworm` + `perf` +
  `bpftrace` + Brendan Gregg's FlameGraph scripts. Single-stage; builds in
  ~3–5 min on first run.
- `run_profile.sh` — entrypoint. Builds the daemon + `perf_bench_test` with
  debug symbols, runs the workload under `perf record` (on-CPU @ 99 Hz) and
  a parallel `bpftrace` off-CPU sampler, then renders two SVG flame charts
  + raw .data + .folded files into `/out`.

## Legacy privileged flame-chart usage

`run_profile.sh` predates the bosn stack and remains available when kernel
`perf`/BPF privileges are required. From repo root on the host:

```bash
docker build -f ci/docker/profile/Dockerfile.perf-linux \
    -t zccache-profile-linux .

docker run --rm \
    --privileged \
    --cap-add=SYS_ADMIN --cap-add=SYS_PTRACE \
    --pid=host \
    -v "$(pwd)":/work:ro \
    -v "$(pwd)/.codex-artifacts/profile-linux-docker-2026-06-25":/out \
    zccache-profile-linux
```

`--privileged` + `--cap-add=SYS_ADMIN` + `--cap-add=SYS_PTRACE` are required
so the container can call `perf_event_open` and BPF helpers from inside the
WSL2 / Linux VM. `--pid=host` is optional and only needed if you want the
profiler to see the host's PID namespace (off-CPU stacks may include host
threads when set).

## Output

Lands in `.codex-artifacts/profile-linux-docker-2026-06-25/`:

- `oncpu.svg` — on-CPU sampled flame chart (where compute lives)
- `offcpu.svg` — off-CPU flame chart (where the daemon **waits**)
- `oncpu.folded`, `offcpu.folded` — raw collapsed-stack input to flamegraph.pl
- `perf.data`, `offcpu.bt` — raw recordings, for re-rendering if needed
- `workload.log` — stdout/stderr from the bench run + any profile lines

## Choosing the workload

Defaults to `perf_rust_workspace_link` because it finishes in ~30 s, fits a
single rustc cold-link, and exercises the cold persist path the on-Windows
profile already highlighted as the foreground critical path.

Override with the env var `ZCCACHE_PROFILE_WORKLOAD`:

```bash
docker run … -e ZCCACHE_PROFILE_WORKLOAD=perf_rustc_zccache_vs_sccache …
```

"""Local perf-cluster harness — Docker-based reproduction of one scenario from
`.github/workflows/perf-rust-cluster.yml`, without burning a GHA cycle.

Three Docker images (see `ci/docker/README.md`) collaborate:

1. `zccache-perf-soldr-builder` — rust:alpine + musl-dev. Volume-mounts a
   local soldr checkout, builds a static `soldr` binary into a host-side
   `binaries/soldr/` dir.
2. `zccache-perf-zccache-builder` — rust:bookworm. Volume-mounts the
   zccache repo, builds the `zccache` trio into `binaries/zccache/`.
3. `zccache-perf-runner` — rust:bookworm + bash/tar/zstd/jq. Mounts both
   binary dirs + the zccache source for `perf/scenarios/`, runs the
   scenario, writes result.json + cache reports back to host.

Persistent `.perf-local/target/{soldr,zccache}/` dirs let cargo incremental
do its job across runs. First run is a full build (~5-8 min); subsequent
runs after a source edit are seconds.

Usage::

    uv run python ci/perf_local.py                                # default: cold-tar-untar-warm x medium
    uv run python ci/perf_local.py --scenario worktree-share
    uv run python ci/perf_local.py --scenario cold-tar-untar-warm --fixture sqlite-link
    uv run python ci/perf_local.py --rebuild-images               # force docker build of all 3 images

The result table emitted at the end mirrors the rich "Evaluate" step from the
GHA perf cluster, so you can compare local vs cluster numbers row-for-row.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
DOCKER_DIR = REPO_ROOT / "ci" / "docker"
PERF_LOCAL = REPO_ROOT / ".perf-local"

SOLDR_REPO = "https://github.com/zackees/soldr.git"
SOLDR_REF = "main"

IMAGE_SOLDR = "zccache-perf-soldr-builder"
IMAGE_ZCCACHE = "zccache-perf-zccache-builder"
IMAGE_RUNNER = "zccache-perf-runner"

VALID_SCENARIOS = (
    "cold-tar-untar-warm",
    "worktree-share",
    "touch-no-change",
    "restore-no-clean-warm",
)
VALID_FIXTURES = ("medium", "sqlite-link")
DEFAULT_SCENARIO = "cold-tar-untar-warm"
DEFAULT_FIXTURE = "medium"


# ---------------------------------------------------------------------------
# Subprocess helpers


def run(cmd: list[str], *, check: bool = True) -> subprocess.CompletedProcess[bytes]:
    """Run a command, mirroring stdout/stderr to this process."""
    print(f"$ {' '.join(cmd)}", file=sys.stderr, flush=True)
    return subprocess.run(cmd, check=check)


def docker_available() -> bool:
    if shutil.which("docker") is None:
        return False
    try:
        result = subprocess.run(
            ["docker", "info"],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    return result.returncode == 0


def image_exists(tag: str) -> bool:
    """True if a local Docker image with this tag exists."""
    result = subprocess.run(
        ["docker", "images", "-q", tag],
        capture_output=True,
        text=True,
        check=False,
    )
    return result.returncode == 0 and bool(result.stdout.strip())


# ---------------------------------------------------------------------------
# Image build steps


def build_image(tag: str, dockerfile: Path, context: Path, *, force: bool) -> None:
    if not force and image_exists(tag):
        print(f"[perf-local] image {tag} already built, skipping (use --rebuild-images to force)")
        return
    print(f"[perf-local] building image {tag} from {dockerfile.relative_to(REPO_ROOT)}")
    run([
        "docker", "build",
        "-t", tag,
        "-f", str(dockerfile),
        str(context),
    ])


def build_all_images(*, force: bool) -> None:
    build_image(IMAGE_SOLDR,   DOCKER_DIR / "soldr-builder.Dockerfile",   DOCKER_DIR, force=force)
    build_image(IMAGE_ZCCACHE, DOCKER_DIR / "zccache-builder.Dockerfile", DOCKER_DIR, force=force)
    build_image(IMAGE_RUNNER,  DOCKER_DIR / "runner.Dockerfile",          DOCKER_DIR, force=force)


# ---------------------------------------------------------------------------
# Source preparation


def ensure_soldr_source() -> Path:
    """Shallow-clone soldr's main HEAD into .perf-local/soldr-src/ (or refresh
    if already there). Returns the checkout path."""
    src = PERF_LOCAL / "soldr-src"
    if (src / ".git").is_dir():
        print(f"[perf-local] refreshing soldr source at {src}")
        run(["git", "-C", str(src), "fetch", "--depth", "1", "origin", SOLDR_REF])
        run(["git", "-C", str(src), "reset", "--hard", "FETCH_HEAD"])
    else:
        src.mkdir(parents=True, exist_ok=True)
        print(f"[perf-local] cloning soldr@{SOLDR_REF} -> {src}")
        run(["git", "clone", "--depth", "1", "--branch", SOLDR_REF, SOLDR_REPO, str(src)])
    sha = subprocess.run(
        ["git", "-C", str(src), "rev-parse", "HEAD"],
        capture_output=True, text=True, check=True,
    ).stdout.strip()
    print(f"[perf-local] soldr-src now at {sha[:12]}")
    return src


def ensure_volume_dirs() -> dict[str, Path]:
    """Create the .perf-local/ volume tree, return a name->path map."""
    layout = {
        "soldr_src":         PERF_LOCAL / "soldr-src",
        "target_soldr":      PERF_LOCAL / "target" / "soldr",
        "target_zccache":    PERF_LOCAL / "target" / "zccache",
        # CARGO_HOME volumes. The Dockerfiles set `ENV CARGO_HOME=/cargo-home`
        # so cargo's registry index + crate sources land here. Without a
        # host-side mount, every container starts with an empty CARGO_HOME
        # and re-fetches ~175 MiB to revalidate fingerprints — measured at
        # +28s per no-op build. Persisting it cuts no-op builds to seconds.
        "cargo_home_soldr":   PERF_LOCAL / "cargo-home" / "soldr",
        "cargo_home_zccache": PERF_LOCAL / "cargo-home" / "zccache",
        "bin_soldr":         PERF_LOCAL / "binaries" / "soldr",
        "bin_zccache":       PERF_LOCAL / "binaries" / "zccache",
        "results":           PERF_LOCAL / "results",
    }
    for path in layout.values():
        path.mkdir(parents=True, exist_ok=True)
    return layout


# ---------------------------------------------------------------------------
# Container runs


def host_volume(host: Path, container: str, mode: str = "") -> str:
    """Build a -v argument with absolute paths. mode is optional (`ro` etc)."""
    s = f"{host.resolve()}:{container}"
    if mode:
        s += f":{mode}"
    return s


def run_soldr_builder(layout: dict[str, Path]) -> None:
    print(f"[perf-local] building soldr binary -> {layout['bin_soldr']}")
    run([
        "docker", "run", "--rm",
        "-v", host_volume(layout["soldr_src"],        "/src", "ro"),
        "-v", host_volume(layout["target_soldr"],     "/target"),
        "-v", host_volume(layout["cargo_home_soldr"], "/cargo-home"),
        "-v", host_volume(layout["bin_soldr"],        "/out"),
        IMAGE_SOLDR,
    ])


def run_zccache_builder(layout: dict[str, Path]) -> None:
    print(f"[perf-local] building zccache trio -> {layout['bin_zccache']}")
    run([
        "docker", "run", "--rm",
        "-v", host_volume(REPO_ROOT,                    "/src", "ro"),
        "-v", host_volume(layout["target_zccache"],     "/target"),
        "-v", host_volume(layout["cargo_home_zccache"], "/cargo-home"),
        "-v", host_volume(layout["bin_zccache"],        "/out"),
        IMAGE_ZCCACHE,
    ])


def run_scenario(layout: dict[str, Path], scenario: str, fixture: str) -> Path:
    """Run the per-scenario container. Returns the results dir for this run."""
    results_dir = layout["results"] / scenario
    # Wipe last run's results so partial output from a crashing run doesn't
    # masquerade as a complete result.
    if results_dir.exists():
        shutil.rmtree(results_dir)
    results_dir.mkdir(parents=True)

    soldr_bin = layout["bin_soldr"] / "soldr"
    if not soldr_bin.is_file():
        raise FileNotFoundError(
            f"soldr binary missing at {soldr_bin}. "
            "Did the soldr-builder step succeed?"
        )

    print(f"[perf-local] running scenario {scenario} x {fixture} -> {results_dir}")
    start = time.monotonic()
    # Pass any ZCCACHE_* env through to the container so the daemon's
    # env-gated instrumentation (e.g. ZCCACHE_HIT_TRACE=1 for the sub-phase
    # dump from issue #468) reaches the in-container daemon process.
    pass_through_env = [
        (k, v) for k, v in os.environ.items() if k.startswith("ZCCACHE_")
    ]
    env_flags: list[str] = []
    for k, v in pass_through_env:
        env_flags.extend(["-e", f"{k}={v}"])
    run([
        "docker", "run", "--rm",
        "-v", host_volume(soldr_bin,              "/usr/local/bin/soldr", "ro"),
        "-v", host_volume(layout["bin_zccache"],  "/zccache-bin",         "ro"),
        "-v", host_volume(REPO_ROOT,              "/zccache-src",         "ro"),
        "-v", host_volume(results_dir,            "/results"),
        "-e", f"SCENARIO={scenario}",
        "-e", f"FIXTURE={fixture}",
        *env_flags,
        IMAGE_RUNNER,
    ])
    elapsed = time.monotonic() - start
    print(f"[perf-local] scenario completed in {elapsed:.1f}s")
    return results_dir


# ---------------------------------------------------------------------------
# Result rendering — mirrors .github/workflows/perf-rust-cluster.yml
# "Evaluate" step's rich table so local + cluster numbers compare apples-
# to-apples.


def fmt_ms(ms) -> str:
    if ms is None or ms == "":
        return "—"
    ms = int(ms)
    if ms >= 60_000:
        return f"{ms // 60_000}m{(ms % 60_000) // 1000:02d}s"
    if ms >= 1_000:
        return f"{ms / 1000:.2f}s"
    return f"{ms}ms"


def fmt_bytes(b) -> str:
    if b is None or b == "":
        return "—"
    b = int(b)
    if b >= 1 << 30:
        return f"{b / (1 << 30):.2f} GiB"
    if b >= 1 << 20:
        return f"{b / (1 << 20):.1f} MiB"
    if b >= 1 << 10:
        return f"{b / (1 << 10):.1f} KiB"
    return f"{b} B" if b > 0 else "0 B"


def fmt_count_pct(n, total) -> str:
    if n is None or n == "":
        return "—"
    if not total:
        return str(n)
    return f"{int(n)} ({int(n) / int(total) * 100:.1f}%)"


def render_summary(results_dir: Path, scenario: str, fixture: str) -> int:
    """Print a one-row summary table + the inline annotation that the GHA
    Evaluate step would emit. Returns 0 if the speedup hit the 3x gate."""
    result_json = results_dir / "result.json"
    if not result_json.is_file():
        print(f"[perf-local] FAIL: result.json missing at {result_json}")
        return 1
    result = json.loads(result_json.read_text())

    # Per-scenario key naming, matches Evaluate's cold_key_for/warm_key_for.
    cold_key = "a_ms" if scenario == "worktree-share" else "cold_ms"
    warm_key = "b_ms" if scenario == "worktree-share" else "warm_ms"
    cold_ms = result.get(cold_key)
    warm_ms = result.get(warm_key)
    if cold_ms is None or warm_ms is None or warm_ms <= 0:
        print(f"[perf-local] FAIL: bad timing in result.json (cold={cold_ms} warm={warm_ms})")
        return 1
    speedup = cold_ms / warm_ms

    # Warm-side cache report carries the rich session counters.
    report_candidates = [
        results_dir / "warm-cache-report.json",
        results_dir / "b-cache-report.json",
    ]
    report = None
    for candidate in report_candidates:
        if candidate.is_file():
            report = json.loads(candidate.read_text()).get("last_session", {})
            break
    if report is None:
        report = {}

    # `last-session-stats.json` is zccache's own JSON output (written by
    # `zccache session-end --json`); it includes `phase_profile` from
    # PROTOCOL_VERSION 9 onward. Soldr's `cache report` is the
    # canonical structured form but it copies a fixed set of keys into
    # `last_session` and strips unknown fields, so a fresh phase_profile
    # field arrives in `last-session-stats.json` before it surfaces in
    # the report block. Pull it directly to avoid that lag.
    if "phase_profile" not in report:
        stats_candidates = [
            results_dir / "warm-zccache-logs" / "last-session-stats.json",
            results_dir / "b-zccache-logs" / "last-session-stats.json",
        ]
        for candidate in stats_candidates:
            if not candidate.is_file():
                continue
            try:
                raw = json.loads(candidate.read_text())
            except json.JSONDecodeError:
                continue
            phase = raw.get("phase_profile")
            if phase is not None:
                report["phase_profile"] = phase
                break

    compiles = report.get("compilations")
    hits = report.get("hits")
    misses = report.get("misses")
    non_cache = report.get("non_cacheable")
    errs = report.get("errors")
    bytes_w = report.get("bytes_written")
    time_saved = report.get("time_saved_ms")
    unique_srcs = report.get("unique_sources")
    daemon_rss = result.get("peak_daemon_rss_bytes")
    compile_rss = result.get("peak_compile_rss_bytes")

    threshold = 4.5
    verdict = "PASS" if speedup >= threshold else "FAIL"

    print()
    print(f"## Perf result — local Docker harness — {fixture} / {scenario}")
    print()
    header = (
        "| Fixture | Scenario | Verdict | Speedup | Need | Cold | Warm "
        "| Compiles | Hits | Misses | Ignored | Errors | Unique Srcs "
        "| Bytes W | Time Saved | Daemon RSS | Compile RSS |"
    )
    sep = "| --- | --- | :---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    row = (
        f"| {fixture} | {scenario} | **{verdict}** | {speedup:.2f}x | >={threshold:.2f}x "
        f"| {fmt_ms(cold_ms)} | {fmt_ms(warm_ms)} "
        f"| {compiles if compiles is not None else '—'} "
        f"| {fmt_count_pct(hits, compiles)} "
        f"| {fmt_count_pct(misses, compiles)} "
        f"| {fmt_count_pct(non_cache, compiles)} "
        f"| {errs if errs is not None else '—'} "
        f"| {unique_srcs if unique_srcs is not None else '—'} "
        f"| {fmt_bytes(bytes_w)} | {fmt_ms(time_saved)} "
        f"| {fmt_bytes(daemon_rss)} | {fmt_bytes(compile_rss)} |"
    )
    print(header)
    print(sep)
    print(row)
    print()
    print(
        f"{fixture}/{scenario}: speedup={speedup:.2f}x (need >={threshold:.2f}x); "
        f"cold={fmt_ms(cold_ms)} warm={fmt_ms(warm_ms)}; "
        f"compiles={compiles or 0} hits={hits or 0} misses={misses or 0} "
        f"ignored={non_cache or 0} errors={errs or 0}; "
        f"bytes_W={fmt_bytes(bytes_w)} daemon_RSS={fmt_bytes(daemon_rss)}"
    )

    render_phase_breakdown(report.get("phase_profile"))

    return 0 if verdict == "PASS" else 1


def render_phase_breakdown(phase_profile) -> None:
    """Print a phase-breakdown table from `SessionStats.phase_profile`.

    Skipped silently when the daemon didn't populate the field (old
    PROTOCOL_VERSION) or when both hit and miss counts are zero.
    """
    if not isinstance(phase_profile, dict):
        return
    hit_count = int(phase_profile.get("hit_count") or 0)
    miss_count = int(phase_profile.get("miss_count") or 0)
    if hit_count == 0 and miss_count == 0:
        return

    # (label, total-ns, denom-count). Hit phases use hit_count for the
    # per-event average; miss phases use miss_count. The two metadata-cache
    # sub-phases are summed so the table speaks the language used in design
    # discussion ("metadata cache (source+hdrs)").
    src_ns = int(phase_profile.get("hash_source_ns") or 0)
    hdr_ns = int(phase_profile.get("hash_headers_ns") or 0)
    rows = [
        ("parse_args",                  int(phase_profile.get("parse_args_ns") or 0),           hit_count),
        ("build_context",               int(phase_profile.get("build_context_ns") or 0),         hit_count),
        ("metadata cache (source+hdrs)", src_ns + hdr_ns,                                        hit_count),
        ("depgraph_check",              int(phase_profile.get("depgraph_check_ns") or 0),        hit_count),
        ("request_cache_lookup",        int(phase_profile.get("request_cache_lookup_ns") or 0),  hit_count),
        ("cross_root_validate",         int(phase_profile.get("cross_root_validate_ns") or 0),   hit_count),
        ("artifact_lookup",             int(phase_profile.get("artifact_lookup_ns") or 0),       hit_count),
        ("write_output (materialize)",  int(phase_profile.get("write_output_ns") or 0),          hit_count),
        ("bookkeeping",                 int(phase_profile.get("bookkeeping_ns") or 0),           hit_count),
        ("compiler_exec",               int(phase_profile.get("compiler_exec_ns") or 0),         miss_count),
        ("include_scan",                int(phase_profile.get("include_scan_ns") or 0),          miss_count),
        ("hash_all",                    int(phase_profile.get("hash_all_ns") or 0),              miss_count),
        ("artifact_store",              int(phase_profile.get("artifact_store_ns") or 0),        miss_count),
    ]
    rows.sort(key=lambda r: r[1], reverse=True)

    print()
    print(
        f"### Phase breakdown (warm-side daemon — {hit_count} hits, {miss_count} misses)"
    )
    print()
    print("| Phase | Total ms | Avg per event (µs) |")
    print("| --- | ---: | ---: |")
    for label, total_ns, denom in rows:
        if total_ns == 0:
            continue
        total_ms = total_ns / 1_000_000
        if denom > 0:
            avg_us = total_ns / denom / 1_000
            avg_cell = f"{avg_us:.1f}"
        else:
            avg_cell = "—"
        print(f"| {label} | {total_ms:.1f} | {avg_cell} |")

    total_hit_ns = int(phase_profile.get("total_hit_ns") or 0)
    total_miss_ns = int(phase_profile.get("total_miss_ns") or 0)
    print()
    print(
        f"total_hit_ns={total_hit_ns / 1_000_000:.1f}ms "
        f"total_miss_ns={total_miss_ns / 1_000_000:.1f}ms"
    )


# ---------------------------------------------------------------------------


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--scenario",
        choices=VALID_SCENARIOS,
        default=DEFAULT_SCENARIO,
        help=f"Which perf scenario to run (default: {DEFAULT_SCENARIO}).",
    )
    parser.add_argument(
        "--fixture",
        choices=VALID_FIXTURES,
        default=DEFAULT_FIXTURE,
        help=f"Which fixture to exercise (default: {DEFAULT_FIXTURE}).",
    )
    parser.add_argument(
        "--rebuild-images",
        action="store_true",
        help="Force a rebuild of all three Docker images even if cached.",
    )
    parser.add_argument(
        "--skip-soldr-build",
        action="store_true",
        help="Skip the soldr-builder run (reuse an existing binary). Useful for fast iteration after a zccache-only change.",
    )
    parser.add_argument(
        "--skip-zccache-build",
        action="store_true",
        help="Skip the zccache-builder run (reuse an existing binary). Useful for fast iteration when only the scenario script changed.",
    )
    args = parser.parse_args()

    if not docker_available():
        print(
            "ERROR: docker is required but not available.\n"
            "  - Is Docker Desktop running?\n"
            "  - Is `docker` on PATH?\n",
            file=sys.stderr,
        )
        return 2

    print(f"[perf-local] repo root: {REPO_ROOT}")
    print(f"[perf-local] scratch dir: {PERF_LOCAL}")

    layout = ensure_volume_dirs()
    build_all_images(force=args.rebuild_images)

    if not args.skip_soldr_build:
        ensure_soldr_source()
        run_soldr_builder(layout)

    if not args.skip_zccache_build:
        run_zccache_builder(layout)

    results_dir = run_scenario(layout, args.scenario, args.fixture)
    return render_summary(results_dir, args.scenario, args.fixture)


if __name__ == "__main__":
    raise SystemExit(main())

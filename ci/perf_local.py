"""Authoritative local Linux Docker performance harness for zccache.

Three Docker images (see `ci/docker/README.md`) collaborate:

1. `zccache-perf-soldr-builder` — rust:alpine + musl-dev. Volume-mounts a
   local soldr checkout, builds a static `soldr` binary into a host-side
   `binaries/soldr/` dir.
2. `zccache-perf-zccache-builder` — the warmed development image used by
   local test, lint, formatting, and shell subcommands.
3. `zccache-perf-runner` — mounts soldr with this checkout's committed
   zccache HEAD embedded, runs a scenario, and writes reports to the host.

Build state — cargo `/target` and `CARGO_HOME` — lives in named Docker
volumes (`zccache-perf-target-{soldr,zccache}` and
`zccache-perf-cargo-home-{soldr,zccache}`), NOT host bind mounts.
Rationale: with bind mounts on Windows hosts, the WSL2 9P translation
rewrites file mtimes per container start, defeating cargo's incremental
fingerprint check — measured at 4–6 min per "no-op" rebuild. Named
volumes live on Linux-native ext4 inside Docker's VFS and give cargo
a stable filesystem; the same no-op rebuild is 1–3 s.

First run is a full cold build (~5–8 min). Subsequent runs after a
source edit are seconds. Wipe a volume with `docker volume rm
zccache-perf-target-zccache` to force a clean start.

Migrating from the older host-bind-mount layout: the previous
`.perf-local/target/{soldr,zccache}/` and `.perf-local/cargo-home/`
host directories are now unused. They can be deleted to reclaim disk:
`rm -rf .perf-local/target .perf-local/cargo-home` — the named Docker
volumes contain the live build state going forward.

Usage::

    uv run --no-project python ci/perf_local.py                    # default: cold-tar-untar-warm x medium
    uv run --no-project python ci/perf_local.py --matrix           # release gate: all 8 rollout cells
    uv run --no-project python ci/perf_local.py --matrix --repeat 5 # repeated distribution audit
    uv run --no-project python ci/perf_local.py --scenario worktree-share
    uv run --no-project python ci/perf_local.py --scenario cold-tar-untar-warm --fixture sqlite-link
    uv run --no-project python ci/perf_local.py --soldr-ref fix/1651-portable-zccache-identity
    uv run --no-project python ci/perf_local.py --jobs 2           # fit an 8 GiB Docker VM
    uv run --no-project python ci/perf_local.py --rebuild-images   # force docker build of all 3 images

    # Ad-hoc cargo in the same warmed target/ volume — much faster than
    # `soldr cargo` on the host because the daemon is undisturbed:
    uv run --no-project python ci/perf_local.py cargo test --lib --no-run
    uv run --no-project python ci/perf_local.py cargo test --release --lib fscache::metadata::tests::mtimes
    uv run --no-project python ci/perf_local.py cargo clippy --workspace -- -D warnings

    # Issue #477: dedicated subcommands that bake in the right `docker run`
    # incantation (named volumes + MSYS_NO_PATHCONV + persistent rustup
    # state). All run inside the same `zccache-perf-zccache-builder` image:
    uv run --no-project python ci/perf_local.py fmt             # cargo fmt --all -- --check
    uv run --no-project python ci/perf_local.py fmt --fix       # cargo fmt --all (rewrites in place)
    uv run --no-project python ci/perf_local.py clippy          # cargo clippy -p zccache --lib --tests -- -D warnings
    uv run --no-project python ci/perf_local.py clippy --workspace --all-targets
    uv run --no-project python ci/perf_local.py test [PATTERN]  # cargo test --lib [PATTERN]
    uv run --no-project python ci/perf_local.py shell           # interactive bash in the builder image

The eight-cell ``--matrix`` run is the sanctioned release gate. GitHub Actions
does not execute wall-clock benchmarks; it only runs deterministic tests and
platform correctness checks.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
DOCKER_DIR = REPO_ROOT / "ci" / "docker"
PERF_LOCAL = REPO_ROOT / ".perf-local"
PERF_THRESHOLDS_PATH = REPO_ROOT / "ci" / "perf_thresholds.json"

SOLDR_REPO = "https://github.com/zackees/soldr.git"
SOLDR_REF = "main"

IMAGE_SOLDR = "zccache-perf-soldr-builder"
IMAGE_ZCCACHE = "zccache-perf-zccache-builder"
IMAGE_RUNNER = "zccache-perf-runner"

# Named Docker volumes for cargo target + CARGO_HOME. Using Docker-managed
# volumes (Linux-native ext4 under the WSL2 backend on Windows hosts)
# instead of host bind mounts gives cargo a stable, fast filesystem for
# the fingerprint check. With bind mounts on Windows, the 9P translation
# layer rewrites mtimes per container start, defeating cargo's
# incremental — measured at 4–6 min per "no-op" rebuild. With named
# volumes, the same rebuild is seconds.
#
# Volumes are auto-created on first reference. They persist across
# container runs (and across `docker system prune`, since they have
# explicit names). Wipe explicitly with `docker volume rm` if needed.
VOLUME_TARGET_SOLDR = "zccache-perf-target-soldr"
VOLUME_TARGET_ZCCACHE = "zccache-perf-target-zccache"
VOLUME_CARGO_HOME_SOLDR = "zccache-perf-cargo-home-soldr"
VOLUME_CARGO_HOME_ZCCACHE = "zccache-perf-cargo-home-zccache"

# Issue #477: persistent volume for rustup state (toolchain + installed
# components). The base zccache-builder image only includes `cargo`; the
# `fmt` / `clippy` subcommands need `rustfmt` + `clippy-driver` which live
# in `$RUSTUP_HOME/toolchains/.../bin/`. With this volume mounted at
# `/root/.rustup`, the one-time `rustup component add` is cached across
# every subsequent invocation — measured at one ~6 s install on first
# call, ~50 ms (cached) on every call after.
VOLUME_RUST_STATE = "zccache-perf-rust-state"

VALID_SCENARIOS = (
    "build-then-check",
    "cold-tar-untar-warm",
    "worktree-share",
    "touch-no-change",
    "restore-no-clean-warm",
)
VALID_FIXTURES = ("medium", "sqlite-link")
ROLLOUT_SCENARIOS = (
    "cold-tar-untar-warm",
    "worktree-share",
    "touch-no-change",
    "restore-no-clean-warm",
)
DEFAULT_SCENARIO = "cold-tar-untar-warm"
DEFAULT_FIXTURE = "medium"

def load_perf_thresholds() -> dict:
    """Load and validate the single source of truth for local timing gates."""
    thresholds = json.loads(PERF_THRESHOLDS_PATH.read_text(encoding="utf-8"))
    if thresholds.get("schema_version") != 1:
        raise ValueError("unsupported perf threshold manifest schema")
    warm_limits = thresholds.get("maximum_warm_ms")
    if not isinstance(warm_limits, dict) or set(warm_limits) != set(ROLLOUT_SCENARIOS):
        raise ValueError("threshold manifest must define every rollout scenario")
    if not isinstance(thresholds.get("minimum_speedup"), (int, float)):
        raise ValueError("threshold manifest minimum_speedup must be numeric")
    return thresholds


PERF_THRESHOLDS = load_perf_thresholds()
LOCAL_MIN_SPEEDUP = float(PERF_THRESHOLDS["minimum_speedup"])
LOCAL_MAX_WARM_MS = PERF_THRESHOLDS["maximum_warm_ms"]
LOCAL_MAX_STAGED_OVERHEAD_MS = int(PERF_THRESHOLDS["maximum_staged_overhead_ms"])
LOCAL_MAX_MATERIALIZATION_COPIED_BYTES = int(
    PERF_THRESHOLDS["maximum_materialization_copied_bytes"]
)


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
    run(
        [
            "docker",
            "build",
            "-t",
            tag,
            "-f",
            str(dockerfile),
            str(context),
        ]
    )


def build_all_images(*, force: bool) -> None:
    build_image(IMAGE_SOLDR, DOCKER_DIR / "soldr-builder.Dockerfile", DOCKER_DIR, force=force)
    build_image(
        IMAGE_ZCCACHE,
        DOCKER_DIR / "zccache-builder.Dockerfile",
        DOCKER_DIR,
        force=force,
    )
    build_image(IMAGE_RUNNER, DOCKER_DIR / "runner.Dockerfile", DOCKER_DIR, force=force)


# ---------------------------------------------------------------------------
# Source preparation


def git_head(repo: Path) -> str:
    """Return the exact commit checked out in ``repo``."""
    return subprocess.run(
        ["git", "-C", str(repo), "rev-parse", "HEAD"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()


def git_is_dirty(repo: Path) -> bool:
    """True when ``repo`` has tracked or untracked working-tree changes."""
    return bool(
        subprocess.run(
            ["git", "-C", str(repo), "status", "--porcelain"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()
    )


def pin_soldr_zccache_source(soldr_src: Path, *, initialize_submodules: bool = True) -> None:
    """Build soldr with the zccache checkout under test embedded in it.

    soldr consumes zccache as a git submodule, so cloning soldr alone is no
    longer enough. Materialize all soldr
    submodules, then move its zccache submodule to this checkout's exact SHA.
    """
    if git_is_dirty(REPO_ROOT):
        raise RuntimeError("the zccache checkout is dirty; commit or stash changes before running perf_local.py so embedded source cannot differ from the requested run")
    zccache_sha = git_head(REPO_ROOT)
    vendored = soldr_src / "_vender" / "zccache"
    if initialize_submodules or not vendored.exists():
        run(
            [
                "git",
                "-C",
                str(soldr_src),
                "submodule",
                "update",
                "--init",
                "--recursive",
            ]
        )
    if vendored.exists() and git_head(vendored) == zccache_sha:
        print(f"[perf-local] embedded zccache already at {zccache_sha[:12]}, skipping source mutation")
        return
    # Fetch locally so unpublished commits can be measured before push.
    run(["git", "-C", str(vendored), "fetch", str(REPO_ROOT), zccache_sha])
    run(["git", "-C", str(vendored), "checkout", "--detach", zccache_sha])
    actual_sha = git_head(vendored)
    if actual_sha != zccache_sha:
        raise RuntimeError(f"failed to pin soldr's embedded zccache: expected {zccache_sha}, found {actual_sha}")


def ensure_soldr_source(soldr_ref: str = SOLDR_REF) -> Path:
    """Refresh the requested soldr ref and embed this zccache checkout.

    ``--soldr-ref`` is the local bridge for cross-repository changes: it lets a
    zccache branch measure against an unmerged soldr fix without editing the
    shallow scratch clone or waiting for a GitHub Actions cycle.
    """
    src = PERF_LOCAL / "soldr-src"
    if (src / ".git").is_dir():
        print(f"[perf-local] resolving soldr@{soldr_ref} at {src}")
        run(["git", "-C", str(src), "fetch", "--depth", "1", "origin", soldr_ref])
        requested_sha = subprocess.run(
            ["git", "-C", str(src), "rev-parse", "FETCH_HEAD"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()
        source_changed = git_head(src) != requested_sha
        if source_changed:
            run(["git", "-C", str(src), "reset", "--hard", requested_sha])
        else:
            print(f"[perf-local] soldr source already at {requested_sha[:12]}, skipping reset")
    else:
        src.mkdir(parents=True, exist_ok=True)
        print(f"[perf-local] cloning soldr@{soldr_ref} -> {src}")
        run(
            [
                "git",
                "clone",
                "--depth",
                "1",
                "--branch",
                soldr_ref,
                SOLDR_REPO,
                str(src),
            ]
        )
        source_changed = True
    pin_soldr_zccache_source(src, initialize_submodules=source_changed)
    sha = git_head(src)
    print(f"[perf-local] soldr-src now at {sha[:12]}")
    return src


def ensure_volume_dirs() -> dict[str, Path]:
    """Create the host-side `.perf-local/` directories (binaries + soldr-src
    + results). Build state — /target and CARGO_HOME — lives in named
    Docker volumes (see `VOLUME_*` constants) instead of host bind mounts;
    we keep host directories only for things that need to be visible from
    the host file system."""
    layout = {
        "soldr_src": PERF_LOCAL / "soldr-src",
        "bin_soldr": PERF_LOCAL / "binaries" / "soldr",
        "results": PERF_LOCAL / "results",
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


def soldr_build_identity(layout: dict[str, Path]) -> dict[str, object]:
    """Identity of every input that can affect the published soldr binary."""
    image = subprocess.run(
        ["docker", "image", "inspect", "--format", "{{.Id}}", IMAGE_SOLDR],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    return {
        "schema_version": 1,
        "soldr_sha": git_head(layout["soldr_src"]),
        "zccache_sha": git_head(REPO_ROOT),
        "builder_image_id": image,
    }


def run_soldr_builder(layout: dict[str, Path], *, force: bool = False) -> None:
    identity = soldr_build_identity(layout)
    binary = layout["bin_soldr"] / "soldr"
    stamp = layout["bin_soldr"] / "build-identity.json"
    if not force and binary.is_file() and stamp.is_file():
        try:
            previous_identity = json.loads(stamp.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            previous_identity = None
        if previous_identity == identity:
            print("[perf-local] soldr binary inputs unchanged, skipping builder")
            return
    print(f"[perf-local] building soldr binary -> {layout['bin_soldr']}")
    run(
        [
            "docker",
            "run",
            "--rm",
            "-v",
            host_volume(layout["soldr_src"], "/src", "ro"),
            "-v",
            f"{VOLUME_TARGET_SOLDR}:/target",
            "-v",
            f"{VOLUME_CARGO_HOME_SOLDR}:/cargo-home",
            "-v",
            host_volume(layout["bin_soldr"], "/out"),
            IMAGE_SOLDR,
        ]
    )
    if not binary.is_file():
        raise FileNotFoundError(f"soldr builder succeeded without publishing {binary}")
    stamp_tmp = stamp.with_suffix(".json.tmp")
    stamp_tmp.write_text(json.dumps(identity, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    stamp_tmp.replace(stamp)


def run_scenario(
    layout: dict[str, Path],
    scenario: str,
    fixture: str,
    jobs: int,
    results_dir: Path | None = None,
) -> Path:
    """Run the per-scenario container. Returns the results dir for this run."""
    results_dir = results_dir or layout["results"] / fixture / scenario
    # Wipe last run's results so partial output from a crashing run doesn't
    # masquerade as a complete result.
    if results_dir.exists():
        shutil.rmtree(results_dir)
    results_dir.mkdir(parents=True)

    soldr_bin = layout["bin_soldr"] / "soldr"
    if not soldr_bin.is_file():
        raise FileNotFoundError(f"soldr binary missing at {soldr_bin}. Did the soldr-builder step succeed?")

    print(f"[perf-local] running scenario {scenario} x {fixture} -> {results_dir}")
    start = time.monotonic()
    # Pass any ZCCACHE_* env through to the container so the daemon's
    # env-gated instrumentation (e.g. ZCCACHE_HIT_TRACE=1 for the sub-phase
    # dump from issue #468) reaches the in-container daemon process.
    pass_through_env = [(k, v) for k, v in os.environ.items() if k.startswith("ZCCACHE_")]
    # Docker Desktop commonly has an 8 GiB VM even when the Windows host has
    # substantially more RAM. An unconstrained medium-fixture build can run
    # enough rustc processes to exhaust that VM and surface os error 12 through
    # soldr. Keep local measurements reproducible and within the selected
    # budget; callers with a larger VM can raise --jobs explicitly.
    # A performance sample must never silently switch to uncached rustc. This
    # is a benchmark-integrity requirement, not a production default.
    env_flags: list[str] = [
        "-e",
        f"CARGO_BUILD_JOBS={jobs}",
        "-e",
        "SOLDR_DAEMON_REQUIRED=1",
    ]
    for k, v in pass_through_env:
        env_flags.extend(["-e", f"{k}={v}"])
    run(
        [
            "docker",
            "run",
            "--rm",
            "-v",
            host_volume(soldr_bin, "/usr/local/bin/soldr", "ro"),
            "-v",
            host_volume(REPO_ROOT, "/zccache-src", "ro"),
            "-v",
            host_volume(results_dir, "/results"),
            "-e",
            f"SCENARIO={scenario}",
            "-e",
            f"FIXTURE={fixture}",
            *env_flags,
            IMAGE_RUNNER,
        ]
    )
    elapsed = time.monotonic() - start
    print(f"[perf-local] scenario completed in {elapsed:.1f}s")
    return results_dir


# ---------------------------------------------------------------------------
# Result rendering for local diagnostics and retained matrix evidence.


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


def validate_infrastructure_result(result: dict, results_dir: Path) -> None:
    """Reject samples contaminated by soldr abort/retry behavior.

    Timing is meaningless when the build silently timed out or retried without
    the cache, so this schema and its artifact-relative evidence are validated
    before any performance threshold.
    """
    if not isinstance(result, dict):
        raise ValueError("infrastructure result must be an object")
    reasons = result.get("invalid_reasons")
    evidence = result.get("soldr_abort_evidence")
    fallback_evidence = result.get("soldr_daemon_fallback_evidence")
    valid = result.get("infrastructure_valid")
    count_names = (
        "soldr_abort_count",
        "soldr_timeout_count",
        "soldr_no_cache_retry_count",
        "soldr_daemon_fallback_count",
    )
    counts = {name: result.get(name) for name in count_names}

    malformed = (
        type(valid) is not bool
        or not isinstance(reasons, list)
        or not all(isinstance(reason, str) for reason in reasons)
        or not isinstance(evidence, list)
        or not evidence
        or not all(isinstance(item, str) and re.fullmatch(r"soldr-aborts-[A-Za-z0-9_-]+[.]jsonl", item) for item in evidence)
        or not isinstance(fallback_evidence, list)
        or not fallback_evidence
        or not all(
            isinstance(item, str)
            and re.fullmatch(r"soldr-daemon-fallbacks-[A-Za-z0-9_-]+[.]jsonl", item)
            for item in fallback_evidence
        )
        or not all(type(value) is int and value >= 0 for value in counts.values())
    )
    if malformed:
        raise ValueError("missing or malformed infrastructure-validity fields")
    if counts["soldr_timeout_count"] > counts["soldr_abort_count"]:
        raise ValueError("timeout count exceeds abort count")
    if counts["soldr_no_cache_retry_count"] > counts["soldr_abort_count"]:
        raise ValueError("no-cache retry count exceeds abort count")
    if valid != (len(reasons) == 0):
        raise ValueError("infrastructure validity and reasons disagree")
    missing = [item for item in evidence if not (results_dir / item).is_file()]
    if missing:
        raise ValueError(f"declared soldr abort evidence is missing: {missing[0]}")
    missing_fallback = [item for item in fallback_evidence if not (results_dir / item).is_file()]
    if missing_fallback:
        raise ValueError(
            f"declared soldr daemon fallback evidence is missing: {missing_fallback[0]}"
        )
    if not valid or any(counts.values()):
        detail = "; ".join(reasons) or "soldr abort detected"
        raise ValueError(
            f"contaminated benchmark sample: {detail}; aborts={counts['soldr_abort_count']}, timeouts={counts['soldr_timeout_count']}, no-cache retries={counts['soldr_no_cache_retry_count']}, daemon fallbacks={counts['soldr_daemon_fallback_count']}"
        )


def _read_session_report(results_dir: Path, names: tuple[str, ...]) -> dict | None:
    for name in names:
        path = results_dir / name
        if not path.is_file():
            continue
        try:
            payload = json.loads(path.read_text())
        except json.JSONDecodeError:
            return None
        session = payload.get("last_session")
        return session if isinstance(session, dict) else None
    return None


def _staged_profile(report: dict | None) -> dict | None:
    if not report:
        return None
    profile = report.get("phase_profile")
    if not isinstance(profile, dict):
        return None
    staged = profile.get("staged")
    return staged if isinstance(staged, dict) else None


def evaluate_rollout_result(results_dir: Path, scenario: str, fixture: str) -> list[str]:
    """Return every hard-gate failure for one sanctioned local matrix cell."""
    failures: list[str] = []
    result_path = results_dir / "result.json"
    if not result_path.is_file():
        return ["result.json missing"]
    try:
        result = json.loads(result_path.read_text())
    except json.JSONDecodeError as error:
        return [f"result.json is malformed: {error}"]
    if not isinstance(result, dict):
        return ["result.json must contain one object"]

    try:
        validate_infrastructure_result(result, results_dir)
    except ValueError as error:
        failures.append(str(error))

    cold_key = "a_ms" if scenario == "worktree-share" else "cold_ms"
    warm_key = "b_ms" if scenario == "worktree-share" else "warm_ms"
    cold_ms = result.get(cold_key)
    warm_ms = result.get(warm_key)
    if type(cold_ms) is not int or type(warm_ms) is not int or warm_ms <= 0:
        failures.append(f"invalid timing fields {cold_key}={cold_ms} {warm_key}={warm_ms}")
    else:
        speedup = cold_ms / warm_ms
        if speedup < LOCAL_MIN_SPEEDUP:
            failures.append(f"speedup {speedup:.2f}x is below {LOCAL_MIN_SPEEDUP:.2f}x")
        warm_limit = LOCAL_MAX_WARM_MS[scenario]
        if warm_limit is not None and warm_ms > warm_limit:
            failures.append(f"warm time {warm_ms}ms exceeds {warm_limit}ms")

    cold_report = _read_session_report(results_dir, ("cold-cache-report.json", "a-cache-report.json"))
    warm_report = _read_session_report(results_dir, ("warm-cache-report.json", "b-cache-report.json"))
    cold_staged = _staged_profile(cold_report)
    warm_staged = _staged_profile(warm_report)
    if cold_staged is None:
        failures.append("missing cold staged telemetry")
        return failures

    cold_timings = cold_staged.get("timings_ns", {})
    cold_counters = cold_staged.get("counters", {})
    if not isinstance(cold_timings, dict) or not isinstance(cold_counters, dict):
        failures.append("malformed cold staged telemetry")
        return failures
    overhead_ns = sum(int(cold_timings.get(name, 0) or 0) for name in ("hashing", "publication", "miss_materialization"))
    overhead_ms = (overhead_ns + 999_999) // 1_000_000
    if int(cold_counters.get("publication_success", 0) or 0) <= 0:
        failures.append("cold path published no staged generations")
    if overhead_ms > LOCAL_MAX_STAGED_OVERHEAD_MS:
        failures.append(f"staged miss overhead {overhead_ms}ms exceeds {LOCAL_MAX_STAGED_OVERHEAD_MS}ms")

    counter_sets = [cold_counters]
    if warm_staged is not None:
        warm_counters = warm_staged.get("counters", {})
        warm_bytes = warm_staged.get("bytes", {})
        if not isinstance(warm_counters, dict) or not isinstance(warm_bytes, dict):
            failures.append("malformed warm staged telemetry")
            return failures
        counter_sets.append(warm_counters)
        copied = int(warm_bytes.get("materialization_copied", 0) or 0)
        tiers = sum(
            int(warm_counters.get(name, 0) or 0)
            for name in (
                "materialize_reflink",
                "materialize_hardlink_shared",
                "materialize_copy",
            )
        )
    elif scenario == "restore-no-clean-warm":
        copied = 0
        tiers = 0
    else:
        failures.append("missing warm staged telemetry")
        return failures

    salvage = sum(int(counters.get("salvage_attempt", 0) or 0) for counters in counter_sets)
    critical = sum(int(counters.get(name, 0) or 0) for counters in counter_sets for name in ("publication_failure", "publication_conflict", "materialize_failure"))
    if salvage != 0 or critical != 0:
        failures.append(f"salvage={salvage} critical_failures={critical}")
    if copied > LOCAL_MAX_MATERIALIZATION_COPIED_BYTES:
        failures.append(f"materialization copied {copied} bytes, max {LOCAL_MAX_MATERIALIZATION_COPIED_BYTES}")
    if scenario == "restore-no-clean-warm":
        if result.get("warm_misses") != 0:
            failures.append(f"restore warm build had cache misses: {result.get('warm_misses')}")
    elif tiers <= 0:
        failures.append("warm build reported no materialization tier")

    return failures


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

    threshold = LOCAL_MIN_SPEEDUP
    verdict = "PASS" if speedup >= threshold else "FAIL"

    print()
    print(f"## Perf result — local Docker harness — {fixture} / {scenario}")
    print()
    header = "| Fixture | Scenario | Verdict | Speedup | Need | Cold | Warm | Compiles | Hits | Misses | Ignored | Errors | Unique Srcs | Bytes W | Time Saved | Daemon RSS | Compile RSS |"
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
        ("parse_args", int(phase_profile.get("parse_args_ns") or 0), hit_count),
        ("build_context", int(phase_profile.get("build_context_ns") or 0), hit_count),
        ("metadata cache (source+hdrs)", src_ns + hdr_ns, hit_count),
        ("depgraph_check", int(phase_profile.get("depgraph_check_ns") or 0), hit_count),
        (
            "request_cache_lookup",
            int(phase_profile.get("request_cache_lookup_ns") or 0),
            hit_count,
        ),
        (
            "cross_root_validate",
            int(phase_profile.get("cross_root_validate_ns") or 0),
            hit_count,
        ),
        (
            "artifact_lookup",
            int(phase_profile.get("artifact_lookup_ns") or 0),
            hit_count,
        ),
        (
            "write_output (materialize)",
            int(phase_profile.get("write_output_ns") or 0),
            hit_count,
        ),
        ("bookkeeping", int(phase_profile.get("bookkeeping_ns") or 0), hit_count),
        ("compiler_exec", int(phase_profile.get("compiler_exec_ns") or 0), miss_count),
        ("include_scan", int(phase_profile.get("include_scan_ns") or 0), miss_count),
        ("hash_all", int(phase_profile.get("hash_all_ns") or 0), miss_count),
        (
            "artifact_store",
            int(phase_profile.get("artifact_store_ns") or 0),
            miss_count,
        ),
    ]
    rows.sort(key=lambda r: r[1], reverse=True)

    print()
    print(f"### Phase breakdown (warm-side daemon — {hit_count} hits, {miss_count} misses)")
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
    print(f"total_hit_ns={total_hit_ns / 1_000_000:.1f}ms total_miss_ns={total_miss_ns / 1_000_000:.1f}ms")


# ---------------------------------------------------------------------------


def build_zccache_docker_cmd(
    *,
    src_mode: str = "ro",
    entrypoint: str | None = None,
    cargo_args: list[str] | None = None,
    bash_script: str | None = None,
    interactive: bool = False,
) -> list[str]:
    """Build the canonical `docker run` invocation for the
    zccache-builder image (issue #477).

    Wires in the four named volumes (target, CARGO_HOME, rustup state)
    that make cargo's fingerprint work on Windows + WSL2, plus the
    `-w /src` working dir so workspace-relative invocations resolve.
    Pure function: no side effects, returns the argv list. The matching
    runner in `run_zccache_docker_cmd` is what actually invokes it.

    Exactly one of `entrypoint` / `bash_script` should drive the command:
    - `entrypoint="cargo"` (or any binary) + `cargo_args=...` → runs that
      entry directly.
    - `bash_script="rustup ... && cargo ..."` → runs the script via
      `/bin/bash -c`. Use this when chaining `rustup component add`
      ahead of the real command.

    `src_mode="rw"` enables write-back to the host repo (for `cargo fmt`
    auto-format). Default `"ro"` matches the existing safe pattern.
    """
    cmd = ["docker", "run", "--rm"]
    if interactive:
        cmd.append("-it")
    cmd += [
        "-v",
        host_volume(REPO_ROOT, "/src", src_mode),
        "-v",
        f"{VOLUME_TARGET_ZCCACHE}:/target",
        "-v",
        f"{VOLUME_CARGO_HOME_ZCCACHE}:/cargo-home",
        "-v",
        f"{VOLUME_RUST_STATE}:/root/.rustup",
        "-w",
        "/src",
    ]
    if bash_script is not None:
        cmd += ["--entrypoint", "/bin/bash", IMAGE_ZCCACHE, "-c", bash_script]
    else:
        cmd += ["--entrypoint", entrypoint or "cargo", IMAGE_ZCCACHE]
        if cargo_args:
            cmd += cargo_args
    return cmd


def run_zccache_docker_cmd(cmd: list[str]) -> int:
    """Print + run a `docker run` command, returning its exit code.
    Sets `MSYS_NO_PATHCONV=1` in the child env so Git-Bash on Windows
    stops translating `/src` to a Windows path."""
    if not image_exists(IMAGE_ZCCACHE):
        print(
            f"[perf-local] image {IMAGE_ZCCACHE} not built yet — run `uv run python ci/perf_local.py --skip-soldr-build` first.",
            file=sys.stderr,
        )
        return 2
    print(f"$ {' '.join(cmd)}", file=sys.stderr, flush=True)
    env = os.environ.copy()
    env.setdefault("MSYS_NO_PATHCONV", "1")
    return subprocess.run(cmd, check=False, env=env).returncode


def run_cargo_in_container(cargo_args: list[str]) -> int:
    """Run an arbitrary `cargo` command inside the zccache-builder image
    against the named target / CARGO_HOME volumes. The repo is mounted
    read-only at /src; cargo's working directory is /src so workspace-
    relative invocations work transparently.

    The named volumes give cargo a stable, fast Linux-native fs for its
    fingerprint check — much faster than the previous host-bind-mount
    layout where the WSL2 9P translation rewrote mtimes per container
    start and forced repeat rebuilds.

    Use this for unit tests, clippy, doc — anything where you'd run
    `cargo X` on the host but you want zccache's daemon to stay
    undisturbed.
    """
    cmd = build_zccache_docker_cmd(entrypoint="cargo", cargo_args=cargo_args)
    return run_zccache_docker_cmd(cmd)


# Issue #477: one-line bash that ensures rustfmt + clippy components are
# installed inside the rustup-state volume. Cached after the first run
# (the volume persists across container instances). The components are
# installed for the toolchain pinned in `rust-toolchain.toml` (or the
# default if no pin file is found at /src).
_ENSURE_RUSTFMT_CLIPPY = "rustup component add rustfmt clippy >/dev/null 2>&1 || rustup component add rustfmt clippy"


def run_fmt(args: list[str]) -> int:
    """`cargo fmt --all -- --check` by default. Pass `--fix` (or
    `--write`) to drop the `--check` so rustfmt rewrites files in
    place; this requires the `/src` mount to be read-write."""
    fix = any(a in ("--fix", "--write") for a in args)
    cargo_cmd = "cargo fmt --all" if fix else "cargo fmt --all -- --check"
    cmd = build_zccache_docker_cmd(
        src_mode="rw" if fix else "ro",
        bash_script=f"{_ENSURE_RUSTFMT_CLIPPY} && {cargo_cmd}",
    )
    return run_zccache_docker_cmd(cmd)


def run_clippy(args: list[str]) -> int:
    """`cargo clippy -p zccache --lib --tests -- -D warnings` by default.
    Any extra args after `clippy` are forwarded verbatim, e.g.
    `clippy --workspace --all-targets`. The `-- -D warnings` tail is
    always appended unless the caller passed their own `--`."""
    has_separator = "--" in args
    cargo_args = ["clippy"]
    if args:
        cargo_args += args
    else:
        cargo_args += ["-p", "zccache", "--lib", "--tests"]
    if not has_separator:
        cargo_args += ["--", "-D", "warnings"]
    script = f"{_ENSURE_RUSTFMT_CLIPPY} && cargo {' '.join(_shell_quote(a) for a in cargo_args)}"
    cmd = build_zccache_docker_cmd(bash_script=script)
    return run_zccache_docker_cmd(cmd)


def run_test(args: list[str]) -> int:
    """`cargo test --lib [PATTERN] [FLAGS...]` inside the container.
    The first non-flag positional is treated as the pattern; `--release`
    is recognised and prefixed before the pattern. Use the raw `cargo`
    subcommand for unusual invocations."""
    test_args = ["test", "--lib"]
    if args:
        test_args += args
    cmd = build_zccache_docker_cmd(entrypoint="cargo", cargo_args=test_args)
    return run_zccache_docker_cmd(cmd)


def run_shell(args: list[str]) -> int:
    """Drop into an interactive bash inside the zccache-builder image
    with all named volumes mounted, the repo bind-mounted read-write,
    and CWD=/src. Useful for one-off debugging."""
    cmd = build_zccache_docker_cmd(
        src_mode="rw",
        entrypoint="/bin/bash",
        cargo_args=list(args),
        interactive=True,
    )
    # Skip the image_exists check — the user gets a clean docker error
    # if the image isn't built yet, which is the right signal here.
    print(f"$ {' '.join(cmd)}", file=sys.stderr, flush=True)
    env = os.environ.copy()
    env.setdefault("MSYS_NO_PATHCONV", "1")
    return subprocess.run(cmd, check=False, env=env).returncode


def run_exec(args: list[str]) -> int:
    """Run a checked-in bash recipe in the warmed Linux builder container."""
    if len(args) != 1 or not args[0].startswith("/src/"):
        print("usage: perf_local.py exec /src/ci/local_pre_pr_steps/<suite>.sh", file=sys.stderr)
        return 2
    cmd = build_zccache_docker_cmd(bash_script=f"bash {_shell_quote(args[0])}")
    return run_zccache_docker_cmd(cmd)


def _shell_quote(arg: str) -> str:
    """Minimal quoting for embedding an argv element inside a `bash -c`
    string. Wraps in single quotes and escapes any embedded single
    quotes — exactly what `shlex.quote` produces, inlined here so we
    don't pull in `shlex` for this one call site."""
    if arg and all(c.isalnum() or c in "@%+=:,./-_" for c in arg):
        return arg
    return "'" + arg.replace("'", "'\"'\"'") + "'"


def _distribution(values: list[int]) -> dict[str, float | int]:
    """Return stable summary statistics for repeated timing samples."""
    ordered = sorted(values)
    median = statistics.median(ordered)
    deviations = [abs(value - median) for value in ordered]
    return {
        "count": len(ordered),
        "min_ms": ordered[0],
        "median_ms": median,
        "p95_ms": ordered[max(0, (len(ordered) * 95 + 99) // 100 - 1)],
        "mad_ms": statistics.median(deviations),
        "max_ms": ordered[-1],
    }


def _write_repeat_summary(
    base_dir: Path,
    samples: list[tuple[Path, dict]],
    scenario: str,
    fixture: str,
) -> None:
    timings: dict[str, list[int]] = {"cold_ms": [], "warm_ms": []}
    for sample_dir, result in samples:
        cold_key = "a_ms" if scenario == "worktree-share" else "cold_ms"
        warm_key = "b_ms" if scenario == "worktree-share" else "warm_ms"
        timings["cold_ms"].append(int(result[cold_key]))
        timings["warm_ms"].append(int(result[warm_key]))
    summary = {
        "schema_version": 1,
        "fixture": fixture,
        "scenario": scenario,
        "samples": [str(path.relative_to(base_dir)) for path, _ in samples],
        "distributions": {name: _distribution(values) for name, values in timings.items()},
    }
    (base_dir / "repeat-summary.json").write_text(
        json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )


def run_rollout_matrix(layout: dict[str, Path], jobs: int, repeat: int) -> int:
    """Run and hard-gate both fixtures across all rollout scenarios."""
    failed_cells: list[str] = []
    for fixture in VALID_FIXTURES:
        for scenario in ROLLOUT_SCENARIOS:
            cell = f"{fixture}/{scenario}"
            base_dir = layout["results"] / fixture / scenario
            if repeat > 1 and base_dir.exists():
                shutil.rmtree(base_dir)
            samples: list[tuple[Path, dict]] = []
            cell_failed = False
            for sample_number in range(1, repeat + 1):
                sample_dir = base_dir if repeat == 1 else base_dir / f"sample-{sample_number:02d}"
                try:
                    results_dir = run_scenario(layout, scenario, fixture, jobs, sample_dir)
                except subprocess.CalledProcessError as error:
                    print(f"[perf-local] FAIL {cell} sample {sample_number}: scenario exited {error.returncode}")
                    cell_failed = True
                    break
                render_summary(results_dir, scenario, fixture)
                failures = evaluate_rollout_result(results_dir, scenario, fixture)
                if failures:
                    cell_failed = True
                    for failure in failures:
                        print(f"[perf-local] HARD-GATE FAIL {cell} sample {sample_number}: {failure}")
                    break
                result = json.loads((results_dir / "result.json").read_text(encoding="utf-8"))
                samples.append((results_dir, result))
            if not cell_failed:
                if repeat > 1:
                    _write_repeat_summary(base_dir, samples, scenario, fixture)
                warm = [int(result["b_ms" if scenario == "worktree-share" else "warm_ms"]) for _, result in samples]
                print(f"[perf-local] HARD-GATE PASS {cell} ({repeat} sample(s), warm median={statistics.median(warm):.0f}ms)")
            else:
                failed_cells.append(cell)

    print()
    if failed_cells:
        print(f"[perf-local] MATRIX FAIL: {len(failed_cells)}/{len(VALID_FIXTURES) * len(ROLLOUT_SCENARIOS)} cells failed: " + ", ".join(failed_cells))
        return 1
    print(f"[perf-local] MATRIX PASS: all {len(VALID_FIXTURES) * len(ROLLOUT_SCENARIOS)} cells passed")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    # Subcommands that dispatch to a single Docker invocation against the
    # zccache-builder image + named volumes. Detected before argparse so
    # flags after the subcommand aren't consumed by this script's parser.
    #
    # `cargo` is the catch-all (PR #475). The named ones (`fmt`, `clippy`,
    # `test`, `shell`) bake in the right `docker run` shape (issue #477)
    # so callers don't have to remember the full incantation.
    SUBCOMMAND_RUNNERS = {
        "cargo": run_cargo_in_container,
        "fmt": run_fmt,
        "clippy": run_clippy,
        "test": run_test,
        "shell": run_shell,
        "exec": run_exec,
    }
    if len(sys.argv) >= 2 and sys.argv[1] in SUBCOMMAND_RUNNERS:
        if not docker_available():
            print(
                "ERROR: docker is required but not available.\n  - Is Docker Desktop running?\n  - Is `docker` on PATH?\n",
                file=sys.stderr,
            )
            return 2
        runner = SUBCOMMAND_RUNNERS[sys.argv[1]]
        return runner(sys.argv[2:])

    parser.add_argument(
        "--scenario",
        choices=VALID_SCENARIOS,
        default=DEFAULT_SCENARIO,
        help=f"Which perf scenario to run (default: {DEFAULT_SCENARIO}).",
    )
    parser.add_argument(
        "--matrix",
        action="store_true",
        help="Run the sanctioned 2-fixture x 4-scenario local Linux gate.",
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
        "--soldr-ref",
        default=SOLDR_REF,
        help=(f"Soldr branch, tag, or commit to build before embedding this zccache checkout (default: {SOLDR_REF})."),
    )
    parser.add_argument(
        "--jobs",
        type=int,
        default=2,
        help="Maximum parallel Cargo jobs inside the scenario container (default: 2).",
    )
    parser.add_argument(
        "--repeat",
        type=int,
        default=1,
        help="Repeat each matrix cell and retain distribution summaries (default: 1).",
    )
    args = parser.parse_args()

    if args.jobs < 1:
        parser.error("--jobs must be at least 1")
    if args.repeat < 1:
        parser.error("--repeat must be at least 1")

    if not docker_available():
        print(
            "ERROR: docker is required but not available.\n  - Is Docker Desktop running?\n  - Is `docker` on PATH?\n",
            file=sys.stderr,
        )
        return 2

    print(f"[perf-local] repo root: {REPO_ROOT}")
    print(f"[perf-local] scratch dir: {PERF_LOCAL}")

    layout = ensure_volume_dirs()
    build_all_images(force=args.rebuild_images)

    ensure_soldr_source(args.soldr_ref)
    run_soldr_builder(layout, force=args.rebuild_images)

    if args.matrix:
        return run_rollout_matrix(layout, args.jobs, args.repeat)

    results_dir = run_scenario(layout, args.scenario, args.fixture, args.jobs)
    return render_summary(results_dir, args.scenario, args.fixture)


if __name__ == "__main__":
    raise SystemExit(main())

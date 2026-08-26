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

    uv run --no-project python ci/perf_local.py
    uv run --no-project python ci/perf_local.py --matrix --repeat 5
    uv run --no-project python ci/perf_local.py --scenario worktree-share
    uv run --no-project python ci/perf_local.py cargo test --lib --no-run

See ``ci/docker/README.md`` for the full command and maintenance reference.
The eight-cell ``--matrix`` run is the sanctioned release gate. GitHub Actions
does not execute wall-clock benchmarks; it only runs deterministic tests and
platform correctness checks.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import tomllib
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
MANAGED_VOLUMES = (
    VOLUME_TARGET_SOLDR,
    VOLUME_TARGET_ZCCACHE,
    VOLUME_CARGO_HOME_SOLDR,
    VOLUME_CARGO_HOME_ZCCACHE,
    VOLUME_RUST_STATE,
)
LABEL_PREFIX = "io.zccache.perf-local"
BUILDER_NAME = "zccache-perf-local"
VOLUME_BUDGET_BYTES = 80 * (1 << 30)
MAX_MANAGED_IMAGES = 6
MAX_BUILDKIT_RECORDS = 100

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


from perf_local_results import (
    LOCAL_MAX_MATERIALIZATION_COPIED_BYTES, LOCAL_MAX_STAGED_OVERHEAD_MS_PER_PUBLICATION,
    LOCAL_MAX_WARM_MS, LOCAL_MIN_SPEEDUP, PERF_THRESHOLDS,
    _distribution, _shell_quote, _write_repeat_summary,
    evaluate_rollout_result, fmt_bytes, fmt_count_pct, fmt_ms,
    remove_previous_results as _remove_previous_results,
    render_phase_breakdown, render_summary,
    run_scenario as _run_scenario,
    validate_infrastructure_result,
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


def managed_volume_create_commands() -> list[list[str]]:
    return [
        [
            "docker",
            "volume",
            "create",
            "--label",
            f"{LABEL_PREFIX}.managed=true",
            volume,
        ]
        for volume in MANAGED_VOLUMES
    ]


def ensure_managed_volumes() -> None:
    for command in managed_volume_create_commands():
        subprocess.run(command, capture_output=True, check=True)


def buildkit_prune_command(*, fast: bool = False) -> list[str]:
    command = [
        "docker",
        "buildx",
        "prune",
        "--builder",
        BUILDER_NAME,
    ]
    if not fast:
        command.extend(["--filter", "until=24h"])
    command.append("--force")
    return command


def image_prune_command(*, fast: bool = False) -> list[str]:
    command = [
        "docker",
        "image",
        "prune",
        "--force",
        "--filter",
        f"label={LABEL_PREFIX}.managed=true",
    ]
    if not fast:
        command.extend(["--filter", "until=24h"])
    return command


def managed_image_count() -> int:
    result = subprocess.run(
        ["docker", "image", "ls", "-q", "--filter", f"label={LABEL_PREFIX}.managed=true"],
        capture_output=True,
        text=True,
        check=False,
    )
    return len(set(result.stdout.split())) if result.returncode == 0 else 0


def buildkit_record_count() -> int:
    result = subprocess.run(
        ["docker", "buildx", "du", "--builder", BUILDER_NAME, "--format", "{{json .}}"],
        capture_output=True,
        text=True,
        check=False,
    )
    return len(result.stdout.splitlines()) if result.returncode == 0 else 0


def _builder_exists() -> bool:
    return (
        subprocess.run(
            ["docker", "buildx", "inspect", BUILDER_NAME],
            capture_output=True,
            check=False,
        ).returncode
        == 0
    )


def _ensure_builder() -> bool:
    if subprocess.run(["docker", "buildx", "version"], capture_output=True, check=False).returncode != 0:
        return False
    if _builder_exists():
        return True
    return (
        subprocess.run(
            ["docker", "buildx", "create", "--name", BUILDER_NAME],
            capture_output=True,
            check=False,
        ).returncode
        == 0
    )


def incremental_docker_gc() -> None:
    """Prune only old zccache-owned artifacts during ordinary harness use."""
    if _builder_exists():
        result = subprocess.run(
            buildkit_prune_command(fast=buildkit_record_count() > MAX_BUILDKIT_RECORDS),
            capture_output=True,
            check=False,
        )
        if result.returncode != 0:
            print("[perf-local] warning: BuildKit GC failed", file=sys.stderr)
    result = subprocess.run(
        image_prune_command(fast=managed_image_count() > MAX_MANAGED_IMAGES),
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        print("[perf-local] warning: managed image GC failed", file=sys.stderr)


def prepare_docker_storage() -> None:
    ensure_managed_volumes()
    incremental_docker_gc()


def volumes_over_budget(usage_bytes: int) -> bool:
    return usage_bytes > VOLUME_BUDGET_BYTES


def volume_usage_command() -> list[str]:
    command = ["docker", "run", "--rm"]
    for index, volume in enumerate(MANAGED_VOLUMES):
        command.extend(["-v", f"{volume}:/managed/{index}:ro"])
    command.extend(
        [
            "--entrypoint",
            "sh",
            IMAGE_ZCCACHE,
            "-c",
            "du -sk /managed/* | awk '{total += $1} END {print total}'",
        ]
    )
    return command


def enforce_volume_budget() -> None:
    """Rotate fixed warm volumes above 80 GiB, but never while they are active."""
    for volume in MANAGED_VOLUMES:
        active = subprocess.run(
            ["docker", "ps", "-q", "--filter", f"volume={volume}"],
            capture_output=True,
            text=True,
            check=False,
        )
        if active.returncode != 0 or active.stdout.strip():
            return
    result = subprocess.run(volume_usage_command(), capture_output=True, text=True, check=False)
    if result.returncode != 0:
        print("[perf-local] warning: unable to measure Docker volume usage", file=sys.stderr)
        return
    try:
        usage = int(result.stdout.strip()) * 1024
    except ValueError:
        print("[perf-local] warning: invalid Docker volume usage result", file=sys.stderr)
        return
    if not volumes_over_budget(usage):
        return
    print("[perf-local] incremental gc: warm volumes exceed 80 GiB; rotating them")
    removed = subprocess.run(["docker", "volume", "rm", "--force", *MANAGED_VOLUMES], check=False)
    if removed.returncode != 0:
        raise RuntimeError("unable to rotate over-budget zccache Docker volumes")
    ensure_managed_volumes()


# ---------------------------------------------------------------------------
# Image build steps


def build_image(tag: str, dockerfile: Path, context: Path, *, force: bool) -> None:
    if not force and image_exists(tag):
        print(f"[perf-local] image {tag} already built, skipping (use --rebuild-images to force)")
        return
    print(f"[perf-local] building image {tag} from {dockerfile.relative_to(REPO_ROOT)}")
    if _ensure_builder():
        build_command = ["docker", "buildx", "build", "--builder", BUILDER_NAME, "--load"]
    else:
        build_command = ["docker", "build"]
    run(
        [
            *build_command,
            "--label",
            f"{LABEL_PREFIX}.managed=true",
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


def git_is_worktree_root(repo: Path) -> bool:
    """True only when ``repo`` is the root of its own Git worktree.

    An uninitialized submodule path can exist as an empty directory. Running
    ``git -C`` there walks up to the superproject, so merely checking that the
    directory exists can make a later checkout mutate the parent repository.
    """
    result = subprocess.run(
        ["git", "-C", str(repo), "rev-parse", "--show-toplevel"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        return False
    return Path(result.stdout.strip()).resolve() == repo.resolve()


def pin_soldr_zccache_source(soldr_src: Path, *, initialize_submodules: bool = True) -> None:
    """Build soldr with the zccache checkout under test embedded in it.

    Older soldr refs consume zccache as a git submodule. Materialize and pin
    that checkout when the ref still declares it. Current soldr releases use
    the registry instead, so write a harness-owned source override for the
    read-only checkout mounted by ``run_soldr_builder``.
    """
    if git_is_dirty(REPO_ROOT):
        raise RuntimeError("the zccache checkout is dirty; commit or stash changes before running perf_local.py so embedded source cannot differ from the requested run")
    gitmodules = soldr_src / ".gitmodules"
    if not gitmodules.is_file() or "_vender/zccache" not in gitmodules.read_text(
        encoding="utf-8"
    ):
        config_path = soldr_src / ".cargo" / "config.toml"
        config_path.parent.mkdir(parents=True, exist_ok=True)
        marker = "# perf-local exact zccache source"
        existing = config_path.read_text(encoding="utf-8") if config_path.is_file() else ""
        existing = existing.split(marker, 1)[0].rstrip()
        override = (
            f"{marker}\n"
            "[patch.crates-io.zccache]\n"
            'path = "/zccache-src/crates/zccache"\n'
        )
        config_path.write_text(f"{existing}\n\n{override}", encoding="utf-8")
        print("[perf-local] soldr registry dependency patched to the local zccache checkout")
        return
    zccache_sha = git_head(REPO_ROOT)
    vendored = soldr_src / "_vender" / "zccache"
    if initialize_submodules or not git_is_worktree_root(vendored):
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
    if not git_is_worktree_root(vendored):
        raise RuntimeError(f"soldr zccache submodule was not initialized at {vendored}")
    if git_head(vendored) == zccache_sha:
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
            # Writable, deliberately. Embedding this checkout's zccache SHA can
            # change zccache's *version* (any `chore(release)` bump does), and
            # soldr's `Cargo.lock` pins it — so cargo must rewrite the lock, and
            # a read-only `/src` fails the build with `failed to write
            # /src/Cargo.lock`. That made the whole local perf gate unusable
            # after every release until someone deleted `.perf-local/soldr-src`.
            #
            # Safe because `soldr-src` is a harness-managed scratch clone, not a
            # user checkout: `ensure_soldr_source` hard-resets it whenever the
            # requested soldr ref moves, so a rewritten lock never accumulates.
            host_volume(layout["soldr_src"], "/src"),
            "-v",
            host_volume(REPO_ROOT, "/zccache-src", "ro"),
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
    lock = tomllib.loads((layout["soldr_src"] / "Cargo.lock").read_text(encoding="utf-8"))
    resolved = [package for package in lock["package"] if package["name"] == "zccache"]
    if len(resolved) != 1 or "source" in resolved[0]:
        raise RuntimeError(
            "soldr builder did not resolve zccache from the mounted checkout under test"
        )
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
    return _run_scenario(
        layout,
        scenario,
        fixture,
        jobs,
        results_dir,
        run_command=run,
        host_volume_spec=host_volume,
        repo_root=REPO_ROOT,
        image_runner=IMAGE_RUNNER,
    )


def remove_previous_results(results_dir: Path) -> None:
    _remove_previous_results(
        results_dir,
        run_command=run,
        host_volume_spec=host_volume,
        image_runner=IMAGE_RUNNER,
    )

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
        prepare_docker_storage()
        if image_exists(IMAGE_ZCCACHE):
            enforce_volume_budget()
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
        "--embedded-matrix",
        action="store_true",
        help="Run the soldr-embedded Rust/C/C++/Emscripten lifecycle matrix.",
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
    parser.add_argument(
        "--resume",
        action="store_true",
        help="Resume and revalidate an existing embedded campaign.",
    )
    parser.add_argument(
        "--language",
        choices=("rust", "c", "cpp", "emscripten"),
        help="Limit --embedded-matrix to one language.",
    )
    args = parser.parse_args()

    if args.jobs < 1:
        parser.error("--jobs must be at least 1")
    if args.repeat < 1:
        parser.error("--repeat must be at least 1")
    if args.matrix and args.embedded_matrix:
        parser.error("--matrix and --embedded-matrix are mutually exclusive")
    if args.language and not args.embedded_matrix:
        parser.error("--language requires --embedded-matrix")
    if args.resume and not args.embedded_matrix:
        parser.error("--resume requires --embedded-matrix")

    if not docker_available():
        print(
            "ERROR: docker is required but not available.\n  - Is Docker Desktop running?\n  - Is `docker` on PATH?\n",
            file=sys.stderr,
        )
        return 2

    prepare_docker_storage()

    print(f"[perf-local] repo root: {REPO_ROOT}")
    print(f"[perf-local] scratch dir: {PERF_LOCAL}")

    layout = ensure_volume_dirs()
    build_all_images(force=args.rebuild_images)
    enforce_volume_budget()

    ensure_soldr_source(args.soldr_ref)
    run_soldr_builder(layout, force=args.rebuild_images)

    if args.matrix:
        return run_rollout_matrix(layout, args.jobs, args.repeat)
    if args.embedded_matrix:
        # Direct `ci/perf_local.py` execution puts `ci/`, not the repository
        # root, first on sys.path. Prefer this checkout over any older installed
        # `ci` package before importing the sibling campaign module.
        sys.path.insert(0, str(REPO_ROOT))
        from ci.perf_embedded import run_embedded_campaign

        output = run_embedded_campaign(
            layout,
            jobs=args.jobs,
            repeat=args.repeat,
            rebuild_images=args.rebuild_images,
            resume=args.resume,
            language=args.language,
        )
        print(f"[perf-local] embedded campaign complete: {output}")
        return 0

    results_dir = run_scenario(layout, args.scenario, args.fixture, args.jobs)
    return render_summary(results_dir, args.scenario, args.fixture)


if __name__ == "__main__":
    raise SystemExit(main())

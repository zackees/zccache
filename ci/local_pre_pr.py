"""Run the fast, Docker-backed checks intended before opening a PR.

This wrapper deliberately delegates container setup, images, and volumes to
``ci/perf_local.py`` so local checks use the same Linux environment as the
authoritative performance harness.
"""

from __future__ import annotations

import argparse
import subprocess
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
PERF_LOCAL = REPO_ROOT / "ci" / "perf_local.py"
UV = "uv"
SUITES = ("integration", "cargo-registry", "wrapper-e2e", "gha-cache")


def run_perf_local(*args: str) -> None:
    command = [UV, "run", "--no-project", "python", str(PERF_LOCAL), *args]
    print("$ " + " ".join(command), flush=True)
    subprocess.run(command, cwd=REPO_ROOT, check=True)


def run_fast_checks() -> None:
    run_perf_local("fmt")
    run_perf_local("clippy")
    run_perf_local("test")


def run_linux_suites(selected: list[str]) -> None:
    features = (
        "zccache/zccache-bin,zccache/daemon-bin,zccache/download-bin,"
        "zccache/download-daemon-bin,zccache/fingerprint-bin,zccache/stamp-bin,"
        "zccache/ci-bin,zccache/crash-tools,zccache/tokio-console,zccache/test-support,"
        "zccache/heap-profile"
    )
    run_perf_local("cargo", "build", "-p", "zccache", "--features", features, "--bin", "zccache")
    for suite in selected:
        run_perf_local("exec", f"/src/ci/local_pre_pr_steps/{suite}.sh")


def run_warm_benchmark() -> None:
    durations: list[float] = []
    for attempt in range(2):
        started = time.perf_counter()
        run_perf_local("cargo", "check", "--workspace", "--all-targets")
        duration = time.perf_counter() - started
        durations.append(duration)
        print(f"[local-pre-pr] cargo check {attempt + 1}: {duration:.2f}s", flush=True)

    warm_seconds = durations[-1]
    if warm_seconds > 30:
        raise SystemExit(
            f"[local-pre-pr] warm no-op budget exceeded: {warm_seconds:.2f}s > 30s"
        )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--benchmark",
        action="store_true",
        help="also measure the two-pass warm no-op cargo check budget",
    )
    parser.add_argument("--suites", default=",".join(SUITES), help="comma-separated Linux suites")
    args = parser.parse_args()
    selected = [item.strip() for item in args.suites.split(",") if item.strip()]
    unknown = sorted(set(selected) - set(SUITES))
    if unknown:
        parser.error(f"unknown suite(s): {', '.join(unknown)}")
    try:
        run_fast_checks()
        run_linux_suites(selected)
        if args.benchmark:
            run_warm_benchmark()
    except subprocess.CalledProcessError as error:
        return error.returncode or 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

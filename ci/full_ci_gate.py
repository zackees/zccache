"""Require completed full CI workflow runs for the exact release commit."""

from __future__ import annotations

import argparse
import json
import os
import urllib.parse
import urllib.request

from ci.ci_mode import selected_workflows

# Names exposed by GitHub's workflow-jobs API for every required full-mode leg.
# A workflow conclusion of success alone also permits every gated job to skip.
REQUIRED_JOBS = {
    "bench-action.yml": {
        "Bare (no cache)",
        "sccache",
        "zccache",
        "zccache (cached target)",
        "Results",
    },
    "broker-stress.yml": {"Broker stress (Linux)"},
    "ci-linux.yml": {
        "x86 / Check",
        "x86 / Test",
        "arm / Check",
        "arm / Test",
        "x86-musl / Check (linux-x86-musl)",
        "arm-musl / Check (linux-arm-musl)",
    },
    "ci-macos.yml": {"macos / Test", "Observe macOS runner queue"},
    "ci-windows.yml": {"x86 / Test", "arm / Test"},
    "ci.yml": {"Formatting", "Dylint", "MSRV (1.95.0)", "Documentation"},
    "clippy.yml": {"Clippy"},
    "coverage.yml": {"Code Coverage"},
    "feature-matrix-check.yml": {"Feature matrix is rendered"},
    "fs-matrix.yml": {
        "matrix (windows-latest)",
        "matrix (ubuntu-latest)",
        "matrix (macos-15)",
        "Observe macOS runner queue",
        "large-cow (windows-latest)",
        "large-cow (ubuntu-latest)",
    },
    "integration.yml": {"Integration (Linux)"},
    "perf-guard.yml": {
        "COW materialization hit budget",
        "Build perf benchmark binary",
        "C speed floor",
        "C++ speed floor",
        "Rust speed floor",
    },
    "python-tests.yml": {"ci/tests", "native Python suites (Linux)"},
    "test-action.yml": {
        "Target snapshot GC unit tests",
        "Linux x86_64",
        "Linux aarch64",
        "macOS ARM",
        "macOS x86_64",
        "Windows x86_64",
        "Windows ARM64",
        "Observe macOS runner queue",
        "No-save mode (Linux)",
        "cargo-registry (ubuntu-24.04)",
        "cargo-registry (macos-15)",
        "cargo-registry (windows-2025)",
        "gha-cache (Linux)",
    },
    "wire-stability.yml": {"Verify wire stability snapshot"},
    "wrapper-e2e.yml": {
        "wrapper-e2e (macos-15)",
        "wrapper-e2e (ubuntu-latest)",
        "wrapper-e2e (windows-latest)",
        "Observe macOS runner queue",
    },
}


def successful_required_jobs(jobs: list[dict], required: set[str]) -> bool:
    for name in required:
        matches = [job for job in jobs if job.get("name") == name]
        if not matches or any(job.get("conclusion") != "success" for job in matches):
            return False
    return True


def has_successful_full_run(
    runs: list[dict], sha: str, required: set[str], get_jobs
) -> bool:
    return any(
        run.get("head_sha") == sha
        and run.get("event") == "workflow_dispatch"
        and run.get("status") == "completed"
        and run.get("conclusion") == "success"
        and successful_required_jobs(get_jobs(run), required)
        for run in runs
    )


def workflow_runs(repository: str, workflow: str, sha: str, token: str) -> list[dict]:
    query = urllib.parse.urlencode(
        {"head_sha": sha, "event": "workflow_dispatch", "per_page": 100}
    )
    url = (
        f"https://api.github.com/repos/{repository}/actions/workflows/"
        f"{workflow}/runs?{query}"
    )
    request = urllib.request.Request(
        url,
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token}",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)["workflow_runs"]


def workflow_jobs(repository: str, run: dict, token: str) -> list[dict]:
    url = f"https://api.github.com/repos/{repository}/actions/runs/{run['id']}/jobs?per_page=100&filter=latest"
    request = urllib.request.Request(
        url,
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token}",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        data = json.load(response)
    if data["total_count"] != len(data["jobs"]):
        raise ValueError("workflow job list is incomplete")
    return data["jobs"]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sha", required=True)
    args = parser.parse_args()
    repository = os.environ["GITHUB_REPOSITORY"]
    token = os.environ["GITHUB_TOKEN"]
    if set(REQUIRED_JOBS) != selected_workflows("full"):
        raise SystemExit("release gate job inventory differs from full CI inventory")
    missing = []
    for workflow in sorted(selected_workflows("full")):
        runs = workflow_runs(repository, workflow, args.sha, token)
        if not has_successful_full_run(
            runs,
            args.sha,
            REQUIRED_JOBS[workflow],
            lambda run: workflow_jobs(repository, run, token),
        ):
            missing.append(workflow)
    if missing:
        raise SystemExit(
            f"Full CI has not passed for exact commit {args.sha}: " + ", ".join(missing)
        )
    print(f"Full CI passed for exact commit {args.sha}")


if __name__ == "__main__":
    main()

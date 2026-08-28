"""Bound queue starvation for GitHub-hosted runner jobs.

Workflow YAML supplies only a repository, run id, runner label, and expected
job names.  This helper owns GitHub API parsing, queue-age accounting, bounded
polling, and the durable step-summary diagnostic required by zccache#1541.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.request
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from enum import Enum
from pathlib import Path
from typing import Protocol


class GitHubApiError(RuntimeError):
    """The Actions API could not provide a state that CI can diagnose."""


class QueueState(Enum):
    MISSING = "missing"
    QUEUED = "queued"
    STARTED = "started"
    COMPLETED = "completed"


@dataclass(frozen=True)
class QueueObservation:
    """One expected job's observable scheduling state."""

    job_name: str
    configured_runner: str
    runner_labels: tuple[str, ...]
    state: QueueState
    status: str
    conclusion: str | None
    queue_age: timedelta | None


class JsonApi(Protocol):
    def get_json(self, path: str) -> object: ...


def _parse_timestamp(value: object, field: str) -> datetime:
    if not isinstance(value, str):
        raise GitHubApiError(f"GitHub Actions response has no string {field}")
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00")).astimezone(UTC)
    except ValueError as error:
        raise GitHubApiError(f"GitHub Actions response has invalid {field}={value!r}") from error


def _mapping(value: object, context: str) -> Mapping[str, object]:
    if not isinstance(value, Mapping):
        raise GitHubApiError(f"GitHub Actions response has non-object {context}")
    return value


class GitHubApi:
    """Small, dependency-free GitHub REST client for workflow queue state."""

    def __init__(self, token: str) -> None:
        self._token = token

    def get_json(self, path: str) -> object:
        request = urllib.request.Request(
            f"https://api.github.com{path}",
            headers={
                "Accept": "application/vnd.github+json",
                "Authorization": f"Bearer {self._token}",
                "X-GitHub-Api-Version": "2022-11-28",
            },
        )
        try:
            with urllib.request.urlopen(request, timeout=20) as response:
                return json.load(response)
        except (urllib.error.HTTPError, urllib.error.URLError, TimeoutError, json.JSONDecodeError) as error:
            raise GitHubApiError(f"GitHub Actions API request {path} failed: {error}") from error


def fetch_workflow_state(api: JsonApi, repository: str, run_id: int) -> tuple[Mapping[str, object], list[Mapping[str, object]]]:
    """Fetch the run and its complete job list, refusing partial API output."""
    run = _mapping(api.get_json(f"/repos/{repository}/actions/runs/{run_id}"), "run")
    jobs: list[Mapping[str, object]] = []
    page = 1
    while True:
        payload = _mapping(
            api.get_json(f"/repos/{repository}/actions/runs/{run_id}/jobs?per_page=100&page={page}"),
            f"jobs page {page}",
        )
        raw_jobs = payload.get("jobs")
        total_count = payload.get("total_count")
        if not isinstance(raw_jobs, list) or not all(isinstance(job, Mapping) for job in raw_jobs) or not isinstance(total_count, int):
            raise GitHubApiError(f"GitHub Actions response has invalid jobs page {page}")
        jobs.extend(raw_jobs)
        if len(jobs) >= total_count:
            return run, jobs
        if not raw_jobs:
            raise GitHubApiError(f"GitHub Actions jobs pagination ended at {len(jobs)} of declared {total_count} jobs")
        page += 1


def observe_once(
    run: Mapping[str, object],
    jobs: Sequence[Mapping[str, object]],
    expected_jobs: Sequence[str],
    *,
    now: datetime,
    configured_runner: str,
) -> list[QueueObservation]:
    """Classify expected job names without relying on mutable presentation text."""
    jobs_by_name = {name: job for job in jobs if isinstance((name := job.get("name")), str)}
    observations: list[QueueObservation] = []
    for name in expected_jobs:
        job = jobs_by_name.get(name)
        if job is None:
            observations.append(
                QueueObservation(
                    name,
                    configured_runner,
                    (),
                    QueueState.MISSING,
                    "missing",
                    None,
                    None,
                )
            )
            continue

        created = _parse_timestamp(job.get("created_at"), f"job {name!r}.created_at")
        started_at = job.get("started_at")
        completed_at = job.get("completed_at")
        labels = job.get("labels")
        runner_labels = tuple(label for label in labels if isinstance(label, str)) if isinstance(labels, list) else ()
        status = str(job.get("status", "unknown"))
        conclusion = job.get("conclusion") if isinstance(job.get("conclusion"), str) else None
        if completed_at is not None:
            state = QueueState.COMPLETED
            queue_end = _parse_timestamp(started_at, f"job {name!r}.started_at") if started_at is not None else now
        elif started_at is not None:
            state = QueueState.STARTED
            queue_end = _parse_timestamp(started_at, f"job {name!r}.started_at")
        else:
            state = QueueState.QUEUED
            queue_end = now
        observations.append(QueueObservation(name, configured_runner, runner_labels, state, status, conclusion, queue_end - created))
    return observations


def queue_violations(observations: Sequence[QueueObservation], maximum: timedelta, *, missing_visible: bool = True) -> list[QueueObservation]:
    """Return only jobs that are still absent/queued beyond the hard bound."""
    return [
        observation
        for observation in observations
        if (observation.state is QueueState.QUEUED and observation.queue_age is not None and observation.queue_age >= maximum) or (observation.state is QueueState.MISSING and missing_visible)
    ]


def render_summary(observations: Sequence[QueueObservation], *, repository: str | None = None, run_id: int | None = None) -> str:
    """Render a compact diagnostic suitable for both logs and step summaries."""
    lines = ["### Hosted runner queue observer", "", "| job | runner | state | queued age | labels |", "| --- | --- | --- | --- | --- |"]
    if repository is not None and run_id is not None:
        lines[2:2] = [f"Workflow: `{repository}` run `{run_id}`", ""]
    for observation in observations:
        labels = ", ".join(observation.runner_labels) or "(not assigned)"
        age = f"{int(observation.queue_age.total_seconds())}s" if observation.queue_age is not None else "not visible"
        lines.append(f"| `{observation.job_name}` | `{observation.configured_runner}` | `{observation.state.value}` | {age} | {labels} |")
    return "\n".join(lines) + "\n"


def _append_summary(text: str) -> None:
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with Path(summary).open("a", encoding="utf-8") as handle:
            handle.write(text)


def _positive_seconds(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return parsed


def _arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--run-id", required=True, type=int)
    parser.add_argument("--job", action="append", required=True, dest="jobs")
    parser.add_argument("--runner", required=True)
    parser.add_argument("--token", required=True)
    parser.add_argument("--max-queue-seconds", type=_positive_seconds, default=900)
    parser.add_argument("--identity-visibility-grace-seconds", type=_positive_seconds, default=120)
    parser.add_argument("--poll-seconds", type=_positive_seconds, default=30)
    return parser.parse_args()


def main() -> int:
    args = _arguments()
    api = GitHubApi(args.token)
    maximum = timedelta(seconds=args.max_queue_seconds)
    visibility_deadline = time.monotonic() + args.identity_visibility_grace_seconds
    while True:
        try:
            run, jobs = fetch_workflow_state(api, args.repository, args.run_id)
            observations = observe_once(run, jobs, args.jobs, now=datetime.now(UTC), configured_runner=args.runner)
        except GitHubApiError as error:
            diagnostic = f"### Hosted runner queue observer\n\nAPI error: `{error}`\n"
            print(diagnostic, file=sys.stderr)
            _append_summary(diagnostic)
            return 1

        summary = render_summary(observations, repository=args.repository, run_id=args.run_id)
        print(summary)
        _append_summary(summary)
        violations = queue_violations(observations, maximum, missing_visible=time.monotonic() >= visibility_deadline)
        if violations:
            print(
                "Hosted runner queue bound exceeded for: " + ", ".join(observation.job_name for observation in violations),
                file=sys.stderr,
            )
            return 1
        if all(observation.state in {QueueState.STARTED, QueueState.COMPLETED} for observation in observations):
            return 0
        time.sleep(args.poll_seconds)


if __name__ == "__main__":
    raise SystemExit(main())

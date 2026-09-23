"""Single source of truth for routine, extended, and release CI coverage.

The workflow selector runs on untrusted event metadata with a read-only token.
It deliberately accepts only the two reviewed labels; unknown ``ci-*`` labels
are errors rather than silently reducing coverage.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path

MINIMAL = frozenset(
    {
        "ci.yml",
        "ci-linux.yml",
        "python-tests.yml",
        "wire-stability.yml",
        "feature-matrix-check.yml",
    }
)
EXTENDED = MINIMAL | frozenset({"integration.yml", "wrapper-e2e.yml"})
FULL = EXTENDED | frozenset(
    {
        "ci-macos.yml",
        "ci-windows.yml",
        "fs-matrix.yml",
        "broker-stress.yml",
        "test-action.yml",
        "perf-guard.yml",
        "coverage.yml",
        "clippy.yml",
        "bench-action.yml",
    }
)


def select_mode(
    event: str,
    labels: list[str],
    requested: str | None = None,
    candidate_sha: str | None = None,
    event_sha: str | None = None,
) -> str:
    unknown = [
        label
        for label in labels
        if label.startswith("ci-") and label not in {"ci-test", "ci-full"}
    ]
    if unknown:
        raise ValueError(f"unknown CI label(s): {', '.join(sorted(unknown))}")
    if event == "workflow_dispatch":
        if not requested and not candidate_sha:
            return "full"
        if requested != "full" or not candidate_sha or candidate_sha != event_sha:
            raise ValueError("full dispatch requires the exact workflow commit SHA")
        return "full"
    if requested:
        raise ValueError("only workflow_dispatch may request a mode directly")
    if event == "pull_request":
        if "ci-full" in labels:
            return "full"
        if "ci-test" in labels:
            return "extended"
        return "minimal"
    if event == "push":
        return "minimal"
    if event == "schedule":
        return "full"
    raise ValueError(f"unsupported CI event: {event}")


def selected_workflows(mode: str) -> frozenset[str]:
    if mode == "minimal":
        return MINIMAL
    if mode == "extended":
        return EXTENDED
    if mode == "full":
        return FULL
    raise ValueError(f"unsupported CI mode: {mode}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workflow", required=True)
    parser.add_argument("--event", required=True)
    parser.add_argument("--event-path", type=Path, required=True)
    parser.add_argument("--sha", required=True)
    parser.add_argument("--mode")
    parser.add_argument("--candidate-sha")
    args = parser.parse_args()
    data = json.loads(args.event_path.read_text())
    labels = [label["name"] for label in data.get("pull_request", {}).get("labels", [])]
    mode = select_mode(
        args.event, labels, args.mode or None, args.candidate_sha or None, args.sha
    )
    selected = args.workflow in selected_workflows(mode)
    output = f"mode={mode}\nselected={str(selected).lower()}\n"
    if path := os.environ.get("GITHUB_OUTPUT"):
        with open(path, "a", encoding="utf-8") as stream:
            stream.write(output)
    else:
        print(output, end="")


if __name__ == "__main__":
    main()

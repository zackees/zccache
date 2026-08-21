"""Host-contention monitoring for standalone performance samples."""

from __future__ import annotations

import subprocess
import time
from collections.abc import Callable


SAMPLE_MONITOR_INTERVAL_SECONDS = 2.0
SAMPLE_CONTAINER_MONITOR_INTERVAL_SECONDS = 10.0


def process_names_outside_container(
    processes: list[tuple[int, int, str]],
    container_pids_before: set[int],
    container_pids_after: set[int],
) -> list[str]:
    excluded_pids = container_pids_before | container_pids_after
    while True:
        descendants = {
            pid
            for pid, parent_pid, _name in processes
            if parent_pid in excluded_pids and pid not in excluded_pids
        }
        if not descendants:
            break
        excluded_pids.update(descendants)
    return [name for pid, _parent_pid, name in processes if pid not in excluded_pids]


def monitor_sample_process(
    process: subprocess.Popen[str],
    container_name: str,
    process_probe: Callable[[bool], list[str]],
    container_probe: Callable[[], list[tuple[str, float]]],
    busy_reasons: Callable[[list[tuple[str, float]], list[str]], list[str]],
) -> tuple[int, list[str]]:
    """Wait for a timed sample while recording any competing host activity."""
    detected: set[str] = set()
    next_container_probe = 0.0
    while True:
        try:
            return_code = process.wait(timeout=SAMPLE_MONITOR_INTERVAL_SECONDS)
            finished = True
        except subprocess.TimeoutExpired:
            finished = False

        try:
            detected.update(busy_reasons([], process_probe(finished)))
        except (RuntimeError, subprocess.SubprocessError) as error:
            detected.add(f"host process monitor failed: {error}")
        now = time.monotonic()
        if finished or now >= next_container_probe:
            try:
                containers = [
                    item for item in container_probe() if item[0] != container_name
                ]
                detected.update(busy_reasons(containers, []))
            except (RuntimeError, subprocess.SubprocessError) as error:
                detected.add(f"container monitor failed: {error}")
            next_container_probe = now + SAMPLE_CONTAINER_MONITOR_INTERVAL_SECONDS
        if finished:
            return return_code, sorted(detected)

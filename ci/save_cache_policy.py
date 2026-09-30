"""Runner-aware ``save-cache`` decision for the zccache GitHub Action (#1783).

``save-cache`` accepts ``auto`` (default), ``true`` and ``false``:

* ``true`` / ``false``: unchanged, always / never save.
* ``auto`` on a GitHub-hosted runner: skip on ``pull_request`` (a PR entry is
  scoped to ``refs/pull/N/merge`` and burns the repository's 10 GB budget),
  save otherwise.
* ``auto`` on a local runner: always save. Local runners (act/bosn) have an
  unscoped, unbudgeted cache server, and fleet CI runs there as
  ``pull_request`` on purpose.

Local-runner definition (mirrors zackees/setup-soldr#537, so the two actions
agree): ``ACT == "true"``, or ``ACTIONS_CACHE_URL`` / ``ACTIONS_RESULTS_URL``
whose host is a loopback, private or link-local address.

CLI (used by ``action.yml``): reads ``INPUT_SAVE_CACHE``, ``GITHUB_EVENT_NAME``
and the runner env, logs one ``save-policy:`` line to stderr and prints the
resolved ``true`` or ``false`` to stdout.
"""

from __future__ import annotations

import ipaddress
import os
import sys
from typing import Mapping
from urllib.parse import urlsplit

CACHE_URL_VARS = ("ACTIONS_CACHE_URL", "ACTIONS_RESULTS_URL")


def parse_mode(raw: str | None) -> str:
    value = (raw or "").strip().lower()
    if not value:
        return "auto"
    if value == "auto":
        return "auto"
    if value in ("1", "true", "yes", "on"):
        return "true"
    if value in ("0", "false", "no", "off"):
        return "false"
    raise ValueError(f"save-cache must be one of auto, true, false (got {raw!r})")


def is_local_host(url: str) -> bool:
    """True when the URL's host is loopback, private or link-local."""
    try:
        host = urlsplit(url.strip()).hostname
    except ValueError:
        return False
    if not host:
        return False
    if host.lower() == "localhost":
        return True
    try:
        addr = ipaddress.ip_address(host)
    except ValueError:
        return False
    return addr.is_loopback or addr.is_private or addr.is_link_local


def detect_runner(env: Mapping[str, str]) -> tuple[str, str]:
    """Return ``("local", why)`` or ``("github", "")``."""
    if env.get("ACT", "").strip().lower() == "true":
        return "local", "ACT=true"
    for var in CACHE_URL_VARS:
        if is_local_host(env.get(var, "")):
            return "local", f"{var} is a local address"
    return "github", ""


def decide(mode: str, event: str, env: Mapping[str, str]) -> tuple[bool, str]:
    """Return ``(save, log_line)`` for an already-parsed ``mode``."""
    if mode in ("true", "false"):
        save = mode == "true"
        return save, f"save-policy: mode={mode} → {'save' if save else 'skip'}"
    runner, why = detect_runner(env)
    if runner == "local":
        return True, f"save-policy: runner=local ({why}) mode=auto → save"
    event = event.strip()
    save = event != "pull_request"
    return save, (
        f"save-policy: runner=github event={event or 'unknown'} mode=auto → "
        f"{'save' if save else 'skip'}"
    )


def main(env: Mapping[str, str] | None = None) -> int:
    env = os.environ if env is None else env
    try:
        mode = parse_mode(env.get("INPUT_SAVE_CACHE"))
    except ValueError as exc:
        print(f"::error::{exc}", file=sys.stderr)
        return 2
    save, line = decide(mode, env.get("GITHUB_EVENT_NAME", ""), env)
    print(line, file=sys.stderr)
    print("true" if save else "false")
    return 0


if __name__ == "__main__":
    # The log line contains a non-ASCII arrow; Windows consoles default to cp1252.
    sys.stderr.reconfigure(encoding="utf-8")
    raise SystemExit(main())

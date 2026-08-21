"""Caller-owned function caching backed by the zccache daemon."""

from __future__ import annotations

import os
from collections.abc import Callable, Iterable, Mapping

from zccache._native import exec_cached as _exec_cached


def exec_cached(
    name: str,
    input_files: Iterable[str | os.PathLike[str]],
    input_env: Mapping[str, str],
    extra_key: bytes,
    runner: Callable[[], bytes],
) -> bytes:
    """Return cached bytes for declared inputs, invoking ``runner`` on a miss.

    ``name`` and ``extra_key`` identify the caller's result schema. File
    contents and the selected environment mapping are hashed by the daemon.
    A daemon or protocol failure raises ``RuntimeError`` and never invokes an
    implicit uncached fallback.
    """

    files = [os.fspath(path) for path in input_files]
    env = list(input_env.items())
    return _exec_cached(name, files, env, bytes(extra_key), runner)

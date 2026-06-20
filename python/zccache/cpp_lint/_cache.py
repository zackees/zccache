"""Per-(TU, item) on-disk JSON cache.

Cache key = blake3(family || tu_fingerprint || item_name ||
hash(item_config) || hash(scope_files) || cache_key_namespace).
Cache value = list of RawResult dicts (success) OR a single
DeterministicFailure dict.

Storage: one file per key, under `<cache_root>/cpp_lint/<key[:2]>/<key>.json`.
Atomic writes via tempfile + os.replace. No locking — the read/write
pairing inside the runner is single-writer per key.

Deterministic failures (PARSE_ERROR, MATCHER_SYNTAX, IWYU_CONFIG,
COMPILE_FLAGS) ARE cached so cold/warm produce identical streams.
Transient failures (TIMEOUT, OOM, SIGNAL, INTERNAL) are NOT cached.
"""

from __future__ import annotations

import hashlib
import json
import os
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any

DETERMINISTIC_ERROR_KINDS = frozenset(
    {"PARSE_ERROR", "MATCHER_SYNTAX", "IWYU_CONFIG", "COMPILE_FLAGS"}
)


def _hasher() -> Any:
    # blake3 is in the issue spec as the daemon-side cache key hash, but
    # we don't want to add a hard third-party dep just to make Python
    # tests deterministic. blake2b with a fixed digest size gives the
    # same shape and is in the stdlib. The on-disk cache lives under a
    # version-namespaced directory anyway (see LintCache.layout), so a
    # future switch to blake3 inside the daemon doesn't break callers.
    return hashlib.blake2b(digest_size=32)


@dataclass(frozen=True)
class CachedRawResult:
    """One result entry as stored in the cache.

    Shape mirrors ResultItem fields directly, minus `cache` (which is
    set to HIT at replay time) and `tu` (which the runner re-fills).
    """

    path: str
    kind: str
    message: str
    item_name: str
    error: bool
    warning: bool
    line: int
    column: int
    extra: dict[str, str]


@dataclass(frozen=True)
class CachedDeterministicFailure:
    """A cached error result. Replays with kind/family preserved."""

    family: str
    item_name: str
    tu_path: str
    error_kind: str
    message: str
    exit_code: int
    extra: dict[str, str]


@dataclass(frozen=True)
class CacheKey:
    """Materialized cache key (32-byte digest as hex)."""

    digest: str


class LintCache:
    """Per-(TU, item) JSON cache rooted at a directory.

    Use ``LintCache(root)`` to open; the directory is created on demand.
    All paths land under ``<root>/cpp_lint/v1/<digest[:2]>/<digest>.json``.
    """

    LAYOUT = "v1"

    def __init__(self, root: Path) -> None:
        self.root = root / "cpp_lint" / self.LAYOUT
        self.root.mkdir(parents=True, exist_ok=True)

    # ----- key construction -----

    @staticmethod
    def make_key(
        family: str,
        tu_fingerprint: bytes,
        item_name: str,
        item_config_hash: bytes,
        scope_files_hash: bytes,
        cache_key_namespace: bytes,
    ) -> CacheKey:
        h = _hasher()
        h.update(family.encode("utf-8"))
        h.update(b"\x00")
        h.update(tu_fingerprint)
        h.update(b"\x00")
        h.update(item_name.encode("utf-8"))
        h.update(b"\x00")
        h.update(item_config_hash)
        h.update(b"\x00")
        h.update(scope_files_hash)
        h.update(b"\x00")
        h.update(cache_key_namespace)
        return CacheKey(digest=h.hexdigest())

    # ----- I/O -----

    def _path_for(self, key: CacheKey) -> Path:
        return self.root / key.digest[:2] / f"{key.digest}.json"

    def get(self, key: CacheKey) -> dict[str, Any] | None:
        """Return the raw cached payload, or None on miss / read error."""
        path = self._path_for(key)
        if not path.is_file():
            return None
        try:
            return json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return None

    def put(self, key: CacheKey, payload: dict[str, Any]) -> None:
        """Atomically write payload as JSON. Best-effort — errors swallowed.

        Determinism: any payload with `kind == "failure"` AND
        `error_kind` outside DETERMINISTIC_ERROR_KINDS is silently
        dropped (we never cache transient failures).
        """
        if (
            payload.get("kind") == "failure"
            and payload.get("error_kind") not in DETERMINISTIC_ERROR_KINDS
        ):
            return
        path = self._path_for(key)
        path.parent.mkdir(parents=True, exist_ok=True)
        try:
            self._atomic_write(path, json.dumps(payload).encode("utf-8"))
        except OSError:
            return

    @staticmethod
    def _atomic_write(path: Path, data: bytes) -> None:
        # Same dir as target so os.replace stays atomic across the same
        # filesystem.
        fd, tmp = tempfile.mkstemp(
            prefix=".tmp-",
            suffix=".json",
            dir=str(path.parent),
        )
        try:
            with os.fdopen(fd, "wb") as fh:
                fh.write(data)
            os.replace(tmp, path)
        except Exception:
            try:
                os.unlink(tmp)
            except OSError:
                pass
            raise


def hash_file_contents(path: Path) -> bytes:
    """blake2b hash of a file's contents. Returns zero bytes if unreadable."""
    h = _hasher()
    try:
        with path.open("rb") as fh:
            while True:
                chunk = fh.read(64 * 1024)
                if not chunk:
                    break
                h.update(chunk)
    except OSError:
        return b"\x00" * 32
    return h.digest()


def hash_bytes(*chunks: bytes) -> bytes:
    """blake2b hash of a sequence of byte chunks with length prefixes.

    Length prefixes prevent ambiguity between (b"ab", b"cd") and
    (b"abc", b"d").
    """
    h = _hasher()
    for chunk in chunks:
        h.update(len(chunk).to_bytes(8, "big"))
        h.update(chunk)
    return h.digest()


def hash_strings(*strs: str) -> bytes:
    return hash_bytes(*[s.encode("utf-8") for s in strs])


__all__ = [
    "DETERMINISTIC_ERROR_KINDS",
    "CacheKey",
    "CachedDeterministicFailure",
    "CachedRawResult",
    "LintCache",
    "hash_bytes",
    "hash_file_contents",
    "hash_strings",
]

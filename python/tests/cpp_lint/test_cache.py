"""Tests for `zccache.cpp_lint._cache.LintCache`."""

from __future__ import annotations

from pathlib import Path

from zccache.cpp_lint._cache import (
    DETERMINISTIC_ERROR_KINDS,
    LintCache,
    hash_bytes,
    hash_file_contents,
    hash_strings,
)


def _key(cache: LintCache, name: str = "x") -> object:
    return cache.make_key(
        family="ast",
        tu_fingerprint=b"\x00" * 32,
        item_name=name,
        item_config_hash=hash_strings("body"),
        scope_files_hash=hash_strings("src/**"),
        cache_key_namespace=b"v1",
    )


def test_make_key_is_deterministic(tmp_path: Path) -> None:
    cache = LintCache(tmp_path)
    k1 = _key(cache)
    k2 = _key(cache)
    assert k1.digest == k2.digest  # type: ignore[attr-defined]


def test_make_key_distinguishes_item_name(tmp_path: Path) -> None:
    cache = LintCache(tmp_path)
    a = _key(cache, "a")
    b = _key(cache, "b")
    assert a.digest != b.digest  # type: ignore[attr-defined]


def test_put_get_round_trip(tmp_path: Path) -> None:
    cache = LintCache(tmp_path)
    key = _key(cache)
    payload = {"kind": "success", "items": [{"path": "x.cpp", "extra": {}}]}
    cache.put(key, payload)
    got = cache.get(key)
    assert got == payload


def test_miss_returns_none(tmp_path: Path) -> None:
    cache = LintCache(tmp_path)
    key = _key(cache, "never-written")
    assert cache.get(key) is None


def test_deterministic_failure_is_cached(tmp_path: Path) -> None:
    cache = LintCache(tmp_path)
    key = _key(cache)
    payload = {
        "kind": "failure",
        "error_kind": "MATCHER_SYNTAX",
        "exit_code": 1,
        "extra": {},
    }
    cache.put(key, payload)
    got = cache.get(key)
    assert got == payload


def test_transient_failure_is_not_cached(tmp_path: Path) -> None:
    cache = LintCache(tmp_path)
    key = _key(cache)
    payload = {
        "kind": "failure",
        "error_kind": "TIMEOUT",
        "exit_code": -1,
        "extra": {},
    }
    cache.put(key, payload)
    assert cache.get(key) is None  # silently dropped


def test_deterministic_error_kinds_set() -> None:
    assert "PARSE_ERROR" in DETERMINISTIC_ERROR_KINDS
    assert "MATCHER_SYNTAX" in DETERMINISTIC_ERROR_KINDS
    assert "IWYU_CONFIG" in DETERMINISTIC_ERROR_KINDS
    assert "COMPILE_FLAGS" in DETERMINISTIC_ERROR_KINDS
    assert "TIMEOUT" not in DETERMINISTIC_ERROR_KINDS
    assert "OOM" not in DETERMINISTIC_ERROR_KINDS


def test_hash_helpers(tmp_path: Path) -> None:
    f = tmp_path / "file.txt"
    f.write_bytes(b"hello")
    h1 = hash_file_contents(f)
    h2 = hash_file_contents(f)
    assert h1 == h2
    assert len(h1) == 32

    # Length prefixes distinguish (b"ab", b"cd") from (b"abc", b"d").
    h_ab_cd = hash_bytes(b"ab", b"cd")
    h_abc_d = hash_bytes(b"abc", b"d")
    assert h_ab_cd != h_abc_d


def test_layout_versioned(tmp_path: Path) -> None:
    cache = LintCache(tmp_path)
    assert cache.LAYOUT == "v1"
    assert (tmp_path / "cpp_lint" / "v1").is_dir()

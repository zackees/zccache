from __future__ import annotations

import tempfile
import time
from pathlib import Path

import pytest


pytest.importorskip("zccache._native")
pytest.importorskip("zccache.watcher._native")
pytest.importorskip("zccache.fingerprint._native")

import zccache
from zccache.client import ZcCacheClient
from zccache.downloader import DownloadApi, FetchResult, FetchState
from zccache.fingerprint import FingerprintCache
from zccache.watcher import FileWatcher


def test_top_level_import_exposes_expected_symbols() -> None:
    assert zccache.FileWatcher is FileWatcher
    assert zccache.FingerprintCache is FingerprintCache
    assert zccache.ZcCacheClient is ZcCacheClient
    assert zccache.DownloadApi is DownloadApi
    assert zccache.FetchResult is FetchResult
    assert zccache.FetchState is FetchState
    assert callable(zccache.exec_cached)


def test_top_level_import_exposes_expected_submodules() -> None:
    from zccache import client, downloader, fingerprint, watcher

    assert client.ZcCacheClient is ZcCacheClient
    assert downloader.DownloadApi is DownloadApi
    assert fingerprint.FingerprintCache is FingerprintCache
    assert watcher.FileWatcher is FileWatcher


def test_file_watcher_detects_cpp_change() -> None:
    with tempfile.TemporaryDirectory() as temp_dir:
        root = Path(temp_dir)
        source = root / "main.cpp"
        source.write_text("int main() { return 0; }\n", encoding="utf-8")

        watcher = FileWatcher(
            root,
            include_globs=["**/*.cpp"],
            debounce_seconds=0.05,
            poll_interval=0.05,
        )
        try:
            source.write_text("int main() { return 1; }\n", encoding="utf-8")
            deadline = time.time() + 2.0
            event = None
            while time.time() < deadline and event is None:
                event = watcher.poll(0.1)
            assert event is not None
            assert str(source.resolve()) in event.paths
        finally:
            watcher.stop()


def test_fingerprint_cache_lifecycle() -> None:
    with tempfile.TemporaryDirectory() as temp_dir:
        root = Path(temp_dir)
        source = root / "main.cpp"
        cache_file = root / ".cache" / "watch.json"
        source.write_text("int main() { return 0; }\n", encoding="utf-8")

        fp = FingerprintCache(cache_file)

        first = fp.check(root=root, include=["**/*.cpp"])
        assert first.should_run is True
        assert first.changed_files

        fp.mark_success()

        second = fp.check(root=root, include=["**/*.cpp"])
        assert second.should_run is False

        source.write_text("int main() { return 1; }\n", encoding="utf-8")
        third = fp.check(root=root, include=["**/*.cpp"])
        assert third.should_run is True
        assert str(source.resolve()) in third.changed_files


def test_client_session_lifecycle() -> None:
    client = ZcCacheClient()
    client.start()
    try:
        status = client.status()
        assert status.version

        session = client.session_start(cwd=".", track_stats=True)
        assert session.session_id

        mid_stats = client.session_stats(session.session_id)
        assert mid_stats is not None

        end_stats = client.session_end(session.session_id)
        assert end_stats is not None
    finally:
        client.stop()

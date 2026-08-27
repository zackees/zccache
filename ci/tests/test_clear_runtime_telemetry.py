from pathlib import Path

from ci.clear_runtime_telemetry import clear_runtime_telemetry


def _write(path: Path, content: str = "telemetry\n") -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def test_cleanup_removes_every_audit_consumed_telemetry_form_only(tmp_path: Path) -> None:
    cache_root = tmp_path / "cache" / "zccache"
    telemetry = [
        cache_root / "history" / "session" / "compile_journal.jsonl",
        cache_root / "logs" / "compile_journal.jsonl.2026-08-27T00-00-00Z",
        cache_root / "sessions" / "request.jsonl",
        cache_root / "logs" / "audit.jsonl",
        cache_root / "logs" / "audit.jsonl.1",
        cache_root / "daemon-state" / "v1" / "logs" / "daemon-lifecycle.log",
        cache_root / "daemon-state" / "v1" / "logs" / "daemon-lifecycle-soldr-dev.log.1",
        cache_root / "daemon-state" / "v1" / "logs" / "daemon.log",
        cache_root / "daemon-state" / "v1" / "logs" / "daemon.log.1",
    ]
    artifacts = [
        cache_root / "artifacts" / "cached-object",
        cache_root / "daemon-state" / "v1" / "blobs" / "cached-artifact",
        cache_root / "logs" / "unrelated.txt",
    ]
    for path in [*telemetry, *artifacts]:
        _write(path)

    removed = clear_runtime_telemetry(cache_root)

    assert set(removed) == set(telemetry)
    assert all(not path.exists() for path in telemetry)
    assert all(path.exists() for path in artifacts)


def test_cleanup_leaves_a_missing_cache_root_unchanged(tmp_path: Path) -> None:
    assert clear_runtime_telemetry(tmp_path / "missing") == []

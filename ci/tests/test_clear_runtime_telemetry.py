import json
from pathlib import Path

from ci.clear_runtime_telemetry import clear_runtime_telemetry

ROOT = Path(__file__).resolve().parents[2]
SOURCE_FIXTURE = ROOT / "ci" / "log_audit_source_fixture.json"


def _write(path: Path, content: str = "telemetry\n") -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def _materialize(cache_root: Path, entries: list[dict[str, str]]) -> list[Path]:
    paths = []
    for entry in entries:
        path = cache_root / entry["path"]
        _write(path, entry["contents"])
        paths.append(path)
    return paths


def test_cleanup_removes_exactly_the_files_the_rust_audit_reads(tmp_path: Path) -> None:
    """#1523: seeded and wrapper-contract telemetry must not reach the audit.

    The fixture is shared with the Rust `zccache-audit` test that pins which
    files `audit-logs` classifies as sources, so this helper and the audit
    cannot drift apart without one of the two tests failing.
    """
    fixture = json.loads(SOURCE_FIXTURE.read_text(encoding="utf-8"))
    cache_root = tmp_path / "cache" / "zccache"
    telemetry = _materialize(cache_root, fixture["telemetry"])
    artifacts = _materialize(cache_root, fixture["artifacts"])

    removed = clear_runtime_telemetry(cache_root)

    assert sorted(removed) == sorted(telemetry)
    assert all(not path.exists() for path in telemetry)
    assert all(path.exists() for path in artifacts)


def test_cleanup_leaves_a_missing_cache_root_unchanged(tmp_path: Path) -> None:
    assert clear_runtime_telemetry(tmp_path / "missing") == []

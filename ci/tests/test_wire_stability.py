from __future__ import annotations

import importlib.util
import sys
from pathlib import Path


MODULE_PATH = Path(__file__).resolve().parents[1] / "check_wire_stability.py"
SPEC = importlib.util.spec_from_file_location("check_wire_stability", MODULE_PATH)
assert SPEC is not None
assert SPEC.loader is not None
check_wire_stability = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = check_wire_stability
SPEC.loader.exec_module(check_wire_stability)


def _schema(tmp_path: Path, reservations: str):
    proto = tmp_path / "sample.proto"
    proto.write_text(
        'syntax = "proto3";\n\nmessage Sample {\n' + reservations + "}\n",
        encoding="utf-8",
    )
    return check_wire_stability.parse_proto(proto)


def _snapshot() -> dict[str, dict[int, tuple[str, str]]]:
    return {"Sample": {7: ("retired_field", "string")}}


def test_snapshot_field_requires_both_reserved_number_and_name(tmp_path: Path) -> None:
    number_only = _schema(tmp_path, "  reserved 7;\n")
    name_only = _schema(tmp_path, '  reserved "retired_field";\n')

    assert check_wire_stability.diff_against_snapshot(_snapshot(), number_only)
    assert check_wire_stability.diff_against_snapshot(_snapshot(), name_only)


def test_snapshot_field_is_safely_retired_by_dual_reservation(tmp_path: Path) -> None:
    schema = _schema(
        tmp_path,
        '  reserved 7;\n  reserved "retired_field";\n',
    )

    assert check_wire_stability.diff_against_snapshot(_snapshot(), schema) == []


def test_untracked_live_field_requires_snapshot_update(tmp_path: Path) -> None:
    schema = _schema(
        tmp_path,
        "  string retired_field = 7;\n  uint64 added_field = 8;\n",
    )

    assert check_wire_stability.diff_against_snapshot(_snapshot(), schema) == [
        "UNTRACKED field: Sample.8 (added_field: uint64)"
    ]


def test_write_snapshot_retains_dual_reserved_historical_field(
    tmp_path: Path, monkeypatch
) -> None:
    snapshot_path = tmp_path / "wire_stability_snapshot.txt"
    snapshot_path.write_text(
        check_wire_stability.SNAPSHOT_HEADER
        + "Sample\t7\tretired_field\tstring\n",
        encoding="utf-8",
    )
    monkeypatch.setattr(check_wire_stability, "SNAPSHOT_PATH", snapshot_path)
    schema = _schema(
        tmp_path,
        '  reserved 7;\n  reserved "retired_field";\n  uint64 current_field = 8;\n',
    )

    check_wire_stability.write_snapshot(schema)

    assert check_wire_stability.read_snapshot() == {
        "Sample": {
            7: ("retired_field", "string"),
            8: ("current_field", "uint64"),
        }
    }


def test_daemon_status_retirements_remain_in_wire_ledger() -> None:
    snapshot = check_wire_stability.read_snapshot()
    schema = check_wire_stability.parse_proto_files(check_wire_stability.PROTO_PATHS)

    assert snapshot["DaemonStatus"][31] == (
        "bincode_requests_by_type",
        "map<string, uint64>",
    )
    assert snapshot["DaemonStatus"][32] == (
        "bincode_request_telemetry_available",
        "bool",
    )
    assert schema.safely_retires("DaemonStatus", 31, "bincode_requests_by_type")
    assert schema.safely_retires(
        "DaemonStatus", 32, "bincode_request_telemetry_available"
    )

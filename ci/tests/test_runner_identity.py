import os
import subprocess
from pathlib import Path

from ci import perf_guard, runner_identity

SAMPLE_LOG = """\
2026-09-30T12:00:01.0000000Z ##[group]Run ci/runner_identity.sh begin
2026-09-30T12:00:01.1000000Z ##[endgroup]
2026-09-30T12:00:01.2000000Z ##[group]runner-identity
2026-09-30T12:00:01.2100000Z runner-identity-begin v=1
2026-09-30T12:00:01.2200000Z runner.cpu_model=AMD EPYC 7763 64-Core Processor
2026-09-30T12:00:01.2300000Z runner.logical_cores=4
2026-09-30T12:00:01.2400000Z runner.mem_total_kb=16374840
2026-09-30T12:00:01.2500000Z runner.kernel=6.11.0-1018-azure
2026-09-30T12:00:01.2600000Z runner.image_os=ubuntu24
2026-09-30T12:00:01.2700000Z runner.image_version=20260921.1.0
2026-09-30T12:00:01.2800000Z runner.name=GitHub Actions 1000123456
2026-09-30T12:00:01.2900000Z runner.arch=X64
2026-09-30T12:00:01.3000000Z runner.steal_ticks_begin=1520
2026-09-30T12:00:01.3100000Z runner.total_ticks_begin=884400
2026-09-30T12:00:01.3200000Z runner.load1_begin=1.25
2026-09-30T12:00:01.3300000Z runner.load5_begin=0.80
2026-09-30T12:00:01.3400000Z runner-identity-end
2026-09-30T12:00:01.3500000Z ##[endgroup]
2026-09-30T12:40:10.0000000Z | Single-file, Cold | 9.000s | 9.500s | 6.000s | ... |
2026-09-30T12:41:00.0000000Z runner-identity-delta v=1 steal_ticks=910 total_ticks=960000 steal_pct=0.095 load1_end=3.91 load5_end=3.40
"""


def test_parse_realistic_log_block_and_delta():
    (block,) = runner_identity.parse_runner_identity(SAMPLE_LOG)
    assert block["cpu_model"] == "AMD EPYC 7763 64-Core Processor"
    assert block["logical_cores"] == 4
    assert block["mem_total_kb"] == 16374840
    assert block["image_version"] == "20260921.1.0"
    assert block["name"] == "GitHub Actions 1000123456"
    assert block["load1_begin"] == 1.25
    assert block["delta"] == {
        "steal_ticks": 910,
        "total_ticks": 960000,
        "steal_pct": 0.095,
        "load1_end": 3.91,
        "load5_end": 3.40,
    }


def test_parse_handles_crlf_unknown_values_and_no_block():
    log = (
        "runner-identity-begin v=1\r\n"
        "runner.cpu_model=unknown\r\nrunner.logical_cores=unknown\r\n"
        "runner-identity-end\r\n"
    )
    (block,) = runner_identity.parse_runner_identity(log)
    assert block["cpu_model"] == "unknown"
    assert block["logical_cores"] == "unknown"
    assert "delta" not in block
    assert runner_identity.parse_runner_identity("nothing here\n") == []
    # Unterminated block is dropped rather than half-parsed.
    assert runner_identity.parse_runner_identity("runner-identity-begin v=1\nrunner.a=b\n") == []


def test_group_by_cpu_model_and_perf_guard_reexport():
    second = SAMPLE_LOG.replace("AMD EPYC 7763 64-Core Processor", "Intel(R) Xeon(R) Platinum 8370C")
    blocks = runner_identity.parse_runner_identity(SAMPLE_LOG + second)
    groups = runner_identity.group_by_cpu_model(blocks)
    assert sorted(groups) == [
        "AMD EPYC 7763 64-Core Processor",
        "Intel(R) Xeon(R) Platinum 8370C",
    ]
    assert perf_guard.parse_runner_identity(SAMPLE_LOG) == runner_identity.parse_runner_identity(
        SAMPLE_LOG
    )


def test_probe_script_output_round_trips(tmp_path: Path):
    script = Path(__file__).resolve().parents[1] / "runner_identity.sh"
    summary = tmp_path / "summary.md"
    env = {
        **os.environ,
        "RUNNER_TEMP": str(tmp_path),
        "GITHUB_STEP_SUMMARY": str(summary),
        "ImageOS": "ubuntu24",
        "ImageVersion": "20260921.1.0",
    }

    def run(mode: str) -> str:
        return subprocess.run(
            ["bash", str(script), mode], env=env, capture_output=True, text=True, check=True
        ).stdout

    out = run("begin") + run("end")
    (block,) = runner_identity.parse_runner_identity(out)
    assert block["image_version"] == "20260921.1.0"
    assert "cpu_model" in block and "delta" in block
    assert block["delta"]["total_ticks"] >= 0
    assert "runner.cpu_model=" in summary.read_text()

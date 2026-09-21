"""Static contracts for Linux CI runner/target topology."""

from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github" / "workflows" / "ci-linux.yml"


def _job(workflow: str, name: str, next_name: str | None = None) -> str:
    start = workflow.index(f"  {name}:\n")
    end = workflow.index(f"\n  {next_name}:\n", start) if next_name else len(workflow)
    return workflow[start:end]


def test_aarch64_musl_cross_check_uses_compatible_x86_host() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")

    native_arm = _job(workflow, "arm", "x86-musl")
    arm_musl = _job(workflow, "arm-musl")

    assert "os: ubuntu-24.04-arm" in native_arm
    assert "os: ubuntu-latest" in arm_musl
    assert "target: aarch64-unknown-linux-musl" in arm_musl

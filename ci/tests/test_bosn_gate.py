"""An isolated Actions result must prove this clean source and selected job."""

import json
from dataclasses import dataclass
from pathlib import Path
from typing import TypeAlias

import pytest

from ci import local_gate

JsonValue: TypeAlias = (
    str | int | float | bool | list["JsonValue"] | dict[str, "JsonValue"] | None
)

RECEIPT = """{"workspace":"/test/source","sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","dirty":null,"state":"done","conclusion":"success","engine":"act","event":"pull_request","workflow":".github/workflows/integration.yml","job":null,"exit_code":0,"tree":{"groups":[{"jobs":[{"job_id":"integration","status":"completed","conclusion":"success","sections":[{"name":"Test (full workspace)","stage":"Main","status":"completed","conclusion":"success"}]}]}]}}"""


def verify(
    text: str, steps: tuple[str, ...] = ("Test (full workspace)",)
) -> str | None:
    return local_gate.bosn_proof_error(
        text,
        workspace=Path("/test/source"),
        head_sha="a" * 40,
        workflow=".github/workflows/integration.yml",
        expected_job="integration",
        selected_job=None,
        required_steps=steps,
    )


def test_clean_completed_actual_job_is_accepted():
    assert verify(RECEIPT) is None


@dataclass(frozen=True)
class ChangedField:
    field: str
    value: str | int


@pytest.mark.parametrize(
    "case",
    (
        ChangedField("workspace", "/other/source"),
        ChangedField("sha", "b" * 40),
        ChangedField("dirty", "changed"),
        ChangedField("state", "running"),
        ChangedField("conclusion", "failure"),
        ChangedField("engine", "other"),
        ChangedField("event", "push"),
        ChangedField("workflow", ".github/workflows/ci.yml"),
        ChangedField("job", "other"),
        ChangedField("exit_code", 1),
    ),
)
def test_wrong_source_or_run_never_proves_a_gate(case: ChangedField):
    boundary: dict[str, JsonValue] = json.loads(RECEIPT)
    boundary[case.field] = case.value
    assert verify(json.dumps(boundary)) is not None


@pytest.mark.parametrize("field", ["workspace", "sha", "dirty", "job", "tree"])
def test_missing_evidence_is_rejected(field: str):
    boundary: dict[str, JsonValue] = json.loads(RECEIPT)
    del boundary[field]
    assert verify(json.dumps(boundary)) is not None


@dataclass(frozen=True)
class JobOutcome:
    status: str
    conclusion: str | None


@pytest.mark.parametrize(
    "case", (JobOutcome("completed", "skipped"), JobOutcome("in_progress", None))
)
def test_skipped_or_incomplete_expected_job_is_rejected(case: JobOutcome):
    text = RECEIPT.replace('"status":"completed"', f'"status":"{case.status}"')
    text = text.replace(
        '"status":"' + case.status + '","conclusion":"success"',
        '"status":"' + case.status + '","conclusion":' + json.dumps(case.conclusion),
    )
    assert verify(text) is not None


def test_success_without_actual_test_execution_is_rejected():
    assert verify(RECEIPT.replace("Test (full workspace)", "Setup only")) is not None


def test_post_action_cannot_prove_main_test_execution():
    assert verify(RECEIPT.replace('"stage":"Main"', '"stage":"Post"')) is not None


def test_missing_actual_job_is_rejected():
    assert (
        verify(RECEIPT.replace('"job_id":"integration"', '"job_id":"ci-mode"'))
        is not None
    )


def test_malformed_and_ambiguous_receipts_are_rejected():
    assert verify("not json") is not None
    assert verify(RECEIPT + "\n" + RECEIPT) is not None


def test_an_empty_execution_contract_cannot_attest():
    assert verify(RECEIPT, steps=()) is not None


def test_job_success_cannot_hide_a_failed_test_step():
    broken = RECEIPT.replace(
        '"stage":"Main","status":"completed","conclusion":"success"',
        '"stage":"Main","status":"completed","conclusion":"failure"',
    )
    assert verify(broken) is not None


@dataclass(frozen=True)
class FidelityCase:
    host: str
    daemon: str
    code: int
    accepted: bool


@pytest.mark.parametrize(
    "case",
    (
        FidelityCase("x86_64", "linux x86_64\n", 0, True),
        FidelityCase("AMD64", "linux amd64\n", 0, True),
        FidelityCase("arm64", "linux x86_64\n", 0, False),
        FidelityCase("x86_64", "linux aarch64\n", 0, False),
        FidelityCase("x86_64", "windows amd64\n", 0, False),
        FidelityCase("x86_64", "linux x86_64\n", 1, False),
        FidelityCase("x86_64", "", 0, False),
    ),
)
def test_emulation_or_unknown_daemon_cannot_prove_tests(case: FidelityCase):
    daemon = local_gate.Captured(case.code, case.daemon)
    assert (local_gate.bosn_fidelity_error(case.host, daemon) is None) == case.accepted


# act2 ends a run whose workflow calls a reusable workflow with
# conclusion="incomplete"/exit 3 ("reusable workflows require qualified
# execution identity") even though act exited 0 and every job succeeded.
# Rejecting that made the gate permanently un-green, so no head could ever be
# attested and no remote job could ever be skipped. Only that exact reason is
# exempt, and only because the per-job evidence still has to stand on its own.


def _receipt(reason: str | None, conclusion: str, exit_code: int) -> str:
    receipt = json.loads(RECEIPT)
    receipt["conclusion"] = conclusion
    receipt["exit_code"] = exit_code
    if reason is not None:
        receipt["reason"] = reason
    return json.dumps(receipt)


LIMITATION = "reusable workflows require qualified execution identity"


def test_reusable_workflow_limitation_with_successful_jobs_is_accepted():
    assert verify(_receipt(LIMITATION, "incomplete", 3)) is None


@pytest.mark.parametrize(
    ("reason", "conclusion", "exit_code"),
    (
        (LIMITATION, "incomplete", 1),
        (LIMITATION, "incomplete", 0),
        ("some other failure", "incomplete", 3),
        (None, "incomplete", 3),
        (LIMITATION, "failure", 3),
        (LIMITATION, "cancelled", 3),
    ),
)
def test_only_the_reusable_workflow_limitation_is_exempt(
    reason: str | None, conclusion: str, exit_code: int
):
    assert verify(_receipt(reason, conclusion, exit_code)) is not None


def test_reusable_workflow_limitation_still_requires_every_job_step():
    receipt = json.loads(_receipt(LIMITATION, "incomplete", 3))
    receipt["tree"]["groups"][0]["jobs"][0]["sections"] = []
    assert verify(json.dumps(receipt)) is not None


@dataclass(frozen=True)
class ReplayCase:
    """`_run` must let the proof decide a replayed workflow, not the raw
    `bosn ci` exit code: act2 exits 3 for its reusable-workflow limitation even
    when every job passed, and that must not fail the lane. A genuinely failed
    job must still fail it."""

    conclusion: str
    exit_code: int
    reason: str | None
    job_conclusion: str
    expect_pass: bool


@pytest.mark.parametrize(
    "case",
    (
        ReplayCase("success", 0, None, "success", True),
        ReplayCase("incomplete", 3, LIMITATION, "success", True),
        ReplayCase("incomplete", 3, "some other failure", "success", False),
        ReplayCase("failure", 1, None, "failure", False),
        ReplayCase("incomplete", 3, LIMITATION, "failure", False),
    ),
)
def test_run_letsthe_proof_decide_the_lane_verdict(case: ReplayCase, monkeypatch):
    receipt = json.loads(RECEIPT)
    receipt["conclusion"] = case.conclusion
    receipt["exit_code"] = case.exit_code
    receipt["workspace"] = str(local_gate.ROOT)
    head_sha = "b" * 40
    receipt["sha"] = head_sha
    if case.reason is not None:
        receipt["reason"] = case.reason
    job = receipt["tree"]["groups"][0]["jobs"][0]
    job["conclusion"] = case.job_conclusion
    for section in job["sections"]:
        section["conclusion"] = case.job_conclusion

    check = local_gate.Check(
        "MSRV check", ("bosn", "ci", "run"), "check", bosn_workflow=".github/workflows/integration.yml",
        bosn_job="integration", selected_job=None,
        required_steps=("Test (full workspace)",),
    )
    monkeypatch.setattr(local_gate, "_run_plain", lambda c: local_gate.Result(c, case.exit_code, 0.0, json.dumps(receipt)))
    # `_run` reads HEAD through run_captured before the proof; hand it this
    # test's receipt sha so the "Bosn executed another commit" guard passes.
    monkeypatch.setattr(
        local_gate, "run_captured",
        lambda argv, *a, **k: (
            local_gate.Captured(0, head_sha) if "rev-parse" in argv
            else local_gate.Captured(0, "linux x86_64")
        ),
    )
    assert (local_gate._run(check).code == 0) == case.expect_pass

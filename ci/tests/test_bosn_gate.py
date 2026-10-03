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

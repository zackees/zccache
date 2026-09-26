import json
from pathlib import Path

from ci import check_cache_budget as budget

FIXTURES = Path(__file__).parent / "fixtures"


def _load(name: str) -> dict:
    return json.loads((FIXTURES / name).read_text(encoding="utf-8"))


def test_over_budget_fails_and_lists_largest_first() -> None:
    data = _load("cache_usage_over.json")
    errors = budget.evaluate(data["usage"], data["caches"])
    assert "exceeds the 9.50 GB budget" in errors[0]
    assert "cook-base-v2-a" in errors[1]
    assert "cook-delta-v2-a-gdeadbeef" in errors[2]
    assert "refs/pull/9/merge" in errors[3]


def test_under_budget_passes() -> None:
    data = _load("cache_usage_under.json")
    assert budget.evaluate(data["usage"], data["caches"]) == []


def test_exact_budget_passes_and_one_byte_over_fails() -> None:
    usage = {"active_caches_size_in_bytes": budget.BUDGET_BYTES}
    assert budget.evaluate(usage, []) == []
    usage["active_caches_size_in_bytes"] += 1
    assert budget.evaluate(usage, []) != []

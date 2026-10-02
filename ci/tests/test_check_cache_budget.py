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


def test_listed_cache_inventory_prevents_false_green_when_usage_lags() -> None:
    usage = {"active_caches_size_in_bytes": budget.BUDGET_BYTES - 1, "active_caches_count": 1}
    caches = [{"key": "new-cache", "ref": "refs/heads/main", "size_in_bytes": budget.BUDGET_BYTES + 1}]
    errors = budget.evaluate(usage, caches)
    assert "exceeds the 9.50 GB budget" in errors[0]
    assert "across at least 1 entries" in errors[0]


def test_usage_endpoint_above_budget_still_fails_when_list_lags() -> None:
    usage = {"active_caches_size_in_bytes": budget.BUDGET_BYTES + 1}
    caches = [{"key": "old-cache", "ref": "refs/heads/main", "size_in_bytes": budget.BUDGET_BYTES - 1}]
    assert budget.evaluate(usage, caches) != []


# --- per-family budgets (#1875) -------------------------------------------

LIVE = "cache_inventory_live_soldr_generation.json"
# The 2026-10-02 live listing's previous soldr generation (#1875): every
# entry created before the 0.9.27 soldr-mini, except the long-lived uv and
# test-registry entries.
GENERATION_EPOCH = "2026-10-02T20:45:00Z"


def _live() -> list[dict]:
    return _load(LIVE)["actions_caches"]


def test_two_soldr_generations_overrun_their_family_budgets() -> None:
    errors = budget.evaluate_families(_live())
    joined = "\n".join(errors)
    for family in ("cook-base", "build-cache", "cargo-registry", "dylint-output", "dylint"):
        assert f"family {family} " in joined, joined


def test_one_generation_fits_every_family_budget() -> None:
    current = [
        cache for cache in _live()
        if cache["created_at"] >= GENERATION_EPOCH or cache["key"].startswith(("setup-uv-", "cargo-registry-"))
    ]
    assert budget.evaluate_families(current) == []


def test_family_budgets_leave_room_for_the_test_binary_cache_under_the_total() -> None:
    # zccache#1550 admits ~0.9 GB of Linux x64 debug test binaries into the
    # build-cache family; the family budgets plus the unbudgeted remainder
    # must still fit the repository budget.
    assert sum(budget.FAMILY_BUDGETS.values()) + budget.OTHER_BUDGET_BYTES <= budget.BUDGET_BYTES
    assert budget.FAMILY_BUDGETS["build-cache"] >= 1_710_000_000 + 900_000_000


def test_pull_request_entries_count_against_no_family() -> None:
    caches = [{"key": "cook-base-v2-x", "ref": "refs/pull/1/merge", "size_in_bytes": 10**12}]
    assert budget.evaluate_families(caches) == []

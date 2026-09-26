"""Tests for FingerprintManager."""

import json
import tempfile
from pathlib import Path

import pytest

from zccache.fingerprint import FingerprintManager, FingerprintResult


def _read_json(tmp: str, name: str) -> dict:
    return json.loads((Path(tmp) / "fingerprint" / f"{name}.json").read_text())


def test_check_first_run_returns_true() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        mgr = FingerprintManager(cache_dir=Path(tmp))
        ran = mgr.check("test", lambda: FingerprintResult(hash="abc"))
        assert ran is True


def test_check_unchanged_returns_false() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        mgr = FingerprintManager(cache_dir=Path(tmp))
        mgr.check("test", lambda: FingerprintResult(hash="abc"))
        mgr.save("test", "success")

        mgr2 = FingerprintManager(cache_dir=Path(tmp))
        ran = mgr2.check("test", lambda: FingerprintResult(hash="abc"))
        assert ran is False


def test_check_changed_returns_true() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        mgr = FingerprintManager(cache_dir=Path(tmp))
        mgr.check("test", lambda: FingerprintResult(hash="abc"))
        mgr.save("test", "success")

        mgr2 = FingerprintManager(cache_dir=Path(tmp))
        ran = mgr2.check("test", lambda: FingerprintResult(hash="def"))
        assert ran is True


def test_check_previous_failure_returns_true() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        mgr = FingerprintManager(cache_dir=Path(tmp))
        mgr.check("test", lambda: FingerprintResult(hash="abc"))
        mgr.save("test", "failure")
        assert _read_json(tmp, "test")["status"] == "failure"

        mgr2 = FingerprintManager(cache_dir=Path(tmp))
        ran = mgr2.check("test", lambda: FingerprintResult(hash="abc"))
        assert ran is True


def test_save_all_persists_status() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        mgr = FingerprintManager(cache_dir=Path(tmp))
        mgr.check("test", lambda: FingerprintResult(hash="abc"))
        mgr.save_all("success", all_ran=True)

        data = _read_json(tmp, "test")
        assert data["hash"] == "abc"
        assert data["status"] == "success"


def test_cache_file_naming_with_build_mode() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        mgr = FingerprintManager(cache_dir=Path(tmp), build_mode="debug")
        mgr.check("cpp_test", lambda: FingerprintResult(hash="x"))
        mgr.save("cpp_test", "success")

        assert (Path(tmp) / "fingerprint" / "cpp_test_debug.json").exists()

        mgr.check("other", lambda: FingerprintResult(hash="y"))
        mgr.save("other", "success")
        assert (Path(tmp) / "fingerprint" / "other.json").exists()


def test_update_test_metadata() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        mgr = FingerprintManager(cache_dir=Path(tmp))
        mgr.check("test", lambda: FingerprintResult(hash="abc"))
        mgr.update_test_metadata("test", 10, 9, 1.5, "unit")
        mgr.save_all("success")

        data = _read_json(tmp, "test")
        assert data["status"] == "success"
        assert data["num_tests_run"] == 10
        assert data["num_tests_passed"] == 9
        assert data["duration_seconds"] == 1.5
        assert data["test_name"] == "unit"


def test_save_all_preserves_prev_metadata_on_skip() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        # First run: record test metadata.
        mgr = FingerprintManager(cache_dir=Path(tmp))
        mgr.check("test", lambda: FingerprintResult(hash="abc"))
        mgr.update_test_metadata("test", 10, 10, 2.0)
        mgr.save_all("success")

        # Second run: cache hit - no new metadata.
        mgr2 = FingerprintManager(cache_dir=Path(tmp))
        mgr2.check("test", lambda: FingerprintResult(hash="abc"))
        mgr2.save_all("success")

        data = _read_json(tmp, "test")
        assert data["num_tests_run"] == 10  # Carried forward from prev


def test_get_prev_fingerprint() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        mgr = FingerprintManager(cache_dir=Path(tmp))
        mgr.check("test", lambda: FingerprintResult(hash="abc"))
        mgr.save("test", "success")

        mgr2 = FingerprintManager(cache_dir=Path(tmp))
        mgr2.check("test", lambda: FingerprintResult(hash="abc"))
        prev = mgr2.get_prev_fingerprint("test")
        assert prev is not None
        assert prev.hash == "abc"
        assert prev.status == "success"


# --- #1650: checking a fingerprint is not completing its operation ---------


def test_save_all_does_not_certify_unexecuted_misses_issue_1650() -> None:
    """Exact reproduction from #1650, using only the pre-existing API.

    Two misses are checked and ``save_all("success")`` is called without
    naming what ran; the next run must not skip ``cpp_test``.
    """
    with tempfile.TemporaryDirectory() as tmp:
        cache = Path(tmp)
        mgr = FingerprintManager(cache)
        assert mgr.check("cpp_test", lambda: FingerprintResult(hash="cpp-v2"))
        assert mgr.check("python_test", lambda: FingerprintResult(hash="py-v2"))

        mgr.save_all("success")

        nxt = FingerprintManager(cache)
        assert nxt.check("cpp_test", lambda: FingerprintResult(hash="cpp-v2")), (
            "cpp_test never ran, but is incorrectly skipped"
        )


def test_save_all_certifies_only_operations_that_ran() -> None:
    """Check A, B and C, run only A, then ``save_all("success")``."""
    with tempfile.TemporaryDirectory() as tmp:
        mgr = FingerprintManager(cache_dir=Path(tmp))
        assert mgr.check("a", lambda: FingerprintResult(hash="a-v2"))
        assert mgr.check("b", lambda: FingerprintResult(hash="b-v2"))
        assert mgr.check("c", lambda: FingerprintResult(hash="c-v2"))

        mgr.mark_ran("a")
        mgr.save_all("success")

        nxt = FingerprintManager(cache_dir=Path(tmp))
        assert nxt.check("a", lambda: FingerprintResult(hash="a-v2")) is False
        assert nxt.check("b", lambda: FingerprintResult(hash="b-v2")) is True
        assert nxt.check("c", lambda: FingerprintResult(hash="c-v2")) is True


def test_unexecuted_miss_keeps_prior_file_dirty() -> None:
    """A changed-but-skipped operation must not overwrite its old entry."""
    with tempfile.TemporaryDirectory() as tmp:
        mgr = FingerprintManager(cache_dir=Path(tmp))
        mgr.check("cpp_test", lambda: FingerprintResult(hash="cpp-v1"))
        mgr.save("cpp_test", "success")

        mgr2 = FingerprintManager(cache_dir=Path(tmp))
        assert mgr2.check("cpp_test", lambda: FingerprintResult(hash="cpp-v2"))
        assert mgr2.check("python_test", lambda: FingerprintResult(hash="py-v2"))
        mgr2.save("python_test", "success")
        mgr2.save_all("success")

        assert _read_json(tmp, "cpp_test_quick")["hash"] == "cpp-v1"
        mgr3 = FingerprintManager(cache_dir=Path(tmp))
        assert mgr3.check("cpp_test", lambda: FingerprintResult(hash="cpp-v2"))
        assert not mgr3.check("python_test", lambda: FingerprintResult(hash="py-v2"))


def test_unexecuted_hit_preserves_prior_status_and_metadata() -> None:
    """A skipped cache hit keeps its verdict even under save_all("failure")."""
    with tempfile.TemporaryDirectory() as tmp:
        mgr = FingerprintManager(cache_dir=Path(tmp))
        mgr.check("a", lambda: FingerprintResult(hash="a1"))
        mgr.update_test_metadata("a", 5, 5, 0.5, "unit-a")
        mgr.save_all("success")

        mgr2 = FingerprintManager(cache_dir=Path(tmp))
        assert not mgr2.check("a", lambda: FingerprintResult(hash="a1"))
        assert mgr2.check("b", lambda: FingerprintResult(hash="b1"))
        mgr2.mark_ran("b")
        mgr2.save_all("failure")

        a = _read_json(tmp, "a")
        assert a["status"] == "success"
        assert a["num_tests_run"] == 5
        assert a["test_name"] == "unit-a"
        assert _read_json(tmp, "b")["status"] == "failure"

        mgr3 = FingerprintManager(cache_dir=Path(tmp))
        assert mgr3.check("a", lambda: FingerprintResult(hash="a1")) is False
        assert mgr3.check("b", lambda: FingerprintResult(hash="b1")) is True


def test_failed_executed_operation_stays_dirty() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        mgr = FingerprintManager(cache_dir=Path(tmp))
        mgr.check("a", lambda: FingerprintResult(hash="a1"))
        mgr.check("b", lambda: FingerprintResult(hash="b1"))
        mgr.save("a", "failure")
        mgr.save("b", "success")

        mgr2 = FingerprintManager(cache_dir=Path(tmp))
        assert mgr2.check("a", lambda: FingerprintResult(hash="a1")) is True
        assert mgr2.check("b", lambda: FingerprintResult(hash="b1")) is False


def test_save_many_certifies_only_named_operations() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        mgr = FingerprintManager(cache_dir=Path(tmp))
        for name in ("a", "b", "c"):
            mgr.check(name, lambda name=name: FingerprintResult(hash=name))
        mgr.save_many(["a", "b"], "success")

        mgr2 = FingerprintManager(cache_dir=Path(tmp))
        assert mgr2.check("a", lambda: FingerprintResult(hash="a")) is False
        assert mgr2.check("b", lambda: FingerprintResult(hash="b")) is False
        assert mgr2.check("c", lambda: FingerprintResult(hash="c")) is True


def test_save_all_all_ran_certifies_every_checked_operation() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        mgr = FingerprintManager(cache_dir=Path(tmp))
        mgr.check("a", lambda: FingerprintResult(hash="a"))
        mgr.check("b", lambda: FingerprintResult(hash="b"))
        mgr.save_all("success", all_ran=True)

        mgr2 = FingerprintManager(cache_dir=Path(tmp))
        assert mgr2.check("a", lambda: FingerprintResult(hash="a")) is False
        assert mgr2.check("b", lambda: FingerprintResult(hash="b")) is False


def test_mark_ran_rejects_unchecked_name() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        mgr = FingerprintManager(cache_dir=Path(tmp))
        mgr.check("a", lambda: FingerprintResult(hash="a"))
        with pytest.raises(KeyError):
            mgr.mark_ran("typo")
        with pytest.raises(KeyError):
            mgr.save("typo", "success")

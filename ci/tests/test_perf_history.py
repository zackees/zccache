from ci import perf_history


def test_manifest_tightening_is_allowed():
    old = {"minimum_speedup": 4.5, "maximum_warm_ms": {"touch": 10_000}}
    new = {"minimum_speedup": 5.0, "maximum_warm_ms": {"touch": 9_000}}

    assert perf_history.manifest_relaxations(old, new) == []
    assert perf_history.validate_ratchet(old, new, None) == []


def test_manifest_relaxation_requires_evidence():
    old = {"minimum_speedup": 4.5, "maximum_warm_ms": {"touch": 10_000}}
    new = {"minimum_speedup": 4.0, "maximum_warm_ms": {"touch": 11_000}}

    assert len(perf_history.manifest_relaxations(old, new)) == 2
    assert perf_history.validate_ratchet(old, new, None)
    assert perf_history.validate_ratchet(
        old,
        new,
        {"issue": "#1093", "samples": ["run-1", "run-2"], "rationale": "runner variance"},
    ) == []


def test_tracked_paths_cover_the_hosted_perf_guard_ratio_floors():
    # #1773: the release-profile ratchet lives in ci/perf_guard.py /
    # ci/perf_floor.py, not the local-matrix manifest this module originally
    # tracked. Those files must get the same git-log provenance trail so
    # `ci/perf_threshold_history.json` records who ratcheted a hosted floor
    # and why.
    assert "ci/perf_guard.py" in perf_history.TRACKED_PATHS
    assert "ci/perf_floor.py" in perf_history.TRACKED_PATHS
    assert "WARM_RATIO_FLOORS" in perf_history.RELAXATION_KEYS


def test_history_inventory_uses_tracked_surfaces(monkeypatch, tmp_path):
    calls = []

    def fake_git(_repo, *args):
        calls.append(args)
        if args[0] == "log":
            return "abc\x00threshold change\nPERF.md\n"
        return "+ minimum_speedup: 4.5"

    monkeypatch.setattr(perf_history, "_git", fake_git)
    rows = perf_history.history_inventory(tmp_path)

    assert rows == [
        {
            "commit": "abc",
            "subject": "threshold change",
            "paths": ["PERF.md"],
            "threshold_lines": ["+ minimum_speedup: 4.5"],
        }
    ]
    assert any(".github/workflows/perf-rust-cluster.yml" in args for call in calls for args in call)

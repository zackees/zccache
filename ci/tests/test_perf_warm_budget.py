"""#1807: sub-millisecond warm hits are gated on an absolute budget, not a ratio.

The samples below are the real per-attempt warm rows of every Perf Guard run
on `main` since the release-profile switch (#1767) whose `perf-guard-*-output`
artifacts are still retained: 15 runs, `(run id, attempt, bare ns, sccache ns,
zccache ns)` copied from each attempt's `ZCCACHE_BENCH_METRIC_V1` record.
The hosted runner class swings the bare baseline 2x while the zccache hit
barely moves, which is what made the ratio floors flaky.
"""

from __future__ import annotations

import json
import statistics
from pathlib import Path

import pytest

from ci import benchmark_stats, perf_floor, perf_guard, perf_precision

ROOT = Path(__file__).resolve().parents[2]

# (run id, attempt, bare ns, sccache ns, zccache ns)
ARCHIVE_SAMPLES = [
    (36493296576, 1, 57884377, 57182010, 410830),
    (36504459992, 1, 59617209, 61301366, 376377),
    (36507856283, 1, 51892456, 53817327, 381804),
    (36519157225, 1, 55903399, 55823638, 425925),
    (36520159059, 1, 36924329, 37281858, 251499),
    (36522679106, 1, 42366534, 42156912, 374252),
    (36522679106, 2, 41828547, 57484122, 312942),
    (36525780332, 1, 59141716, 58898430, 388734),
    (36529823748, 1, 50126859, 50483148, 325083),
    (36540216239, 1, 62091011, 60733129, 375957),
    (36679149864, 1, 53893142, 53219242, 382237),
    (36683332751, 1, 53721658, 54304048, 405760),
    (36724711217, 1, 55466024, 53741549, 383917),
    (36732975369, 1, 50214584, 49480538, 311159),
    (36735417112, 1, 37961945, 36908099, 330070),
    (36735417112, 2, 35909275, 36039403, 359628),
    (36735417112, 3, 36818880, 36018392, 337931),
]

DRIVER_SAMPLES = [
    (36493296576, 1, 60189688, 64295085, 374367),
    (36495649775, 1, 59526281, 64176641, 329037),
    (36504459992, 1, 59194076, 63595185, 346097),
    (36507856283, 1, 44468311, 47557845, 431692),
    (36519157225, 1, 59395461, 63853072, 354905),
    (36520159059, 1, 59151416, 63151662, 363207),
    (36522679106, 1, 59157166, 63836199, 357769),
    (36525780332, 1, 56478738, 60413550, 305781),
    (36529823748, 1, 66767677, 70061679, 388843),
    (36540216239, 1, 70402572, 74300866, 352041),
    (36679149864, 1, 55882215, 59554605, 293179),
    (36683332751, 1, 60314507, 64556848, 359461),
    (36724711217, 1, 63161113, 66461160, 377827),
    (36732975369, 1, 61248858, 65421517, 399135),
    (36735417112, 1, 46058251, 50761777, 234318),
]

RSP_SAMPLES = [
    (36493296576, 1, 9913505928, 9946544788, 19319480),
    (36495649775, 1, 9690817960, 9726884628, 19076697),
    (36504459992, 1, 9665392032, 9626466869, 18109352),
    (36507856283, 1, 4798743455, 4867245540, 11116852),
    (36507856283, 2, 5013684530, 4817652094, 10660165),
    (36507856283, 3, 4671200922, 4702976084, 11321693),
    (36519157225, 1, 9982890377, 10011827621, 18975199),
    (36520159059, 1, 9581191674, 9598930106, 18860657),
    (36522679106, 1, 9871859436, 9702835735, 18518900),
    (36525780332, 1, 7780570091, 7830307976, 15679925),
    (36529823748, 1, 8125354212, 8130722875, 18463019),
    (36540216239, 1, 8379373107, 8408645494, 18351271),
    (36679149864, 1, 6353295552, 6354321665, 13375534),
    (36679149864, 2, 6412288288, 6456807321, 13818752),
    (36683332751, 1, 9823985358, 9835467652, 18291056),
    (36724711217, 1, 9948325291, 10096785796, 19650447),
    (36732975369, 1, 9848103360, 10028236356, 18835426),
    (36735417112, 1, 4475970693, 4451062998, 11489596),
    (36735417112, 2, 4425239199, 4467736460, 11604383),
    (36735417112, 3, 4527557361, 4540863055, 11813253),
]

# The ratio floors these rows had before #1807 (ci/perf_floor.py at 97a677d9).
OLD_RATIO_FLOORS = {
    ("c-static-library-link", "bare"): 117.5,
    ("c-static-library-link", "sccache"): 115.8,
    ("cpp-driver-link", "bare"): 134.1,
    ("cpp-driver-link", "sccache"): 143.3,
    ("cpp-response-file", "bare"): 411.2,
    ("cpp-response-file", "sccache"): 408.5,
}

KINDS = {
    "archive": {
        "header": "## C Static-Library Link Benchmark: 50 .o inputs, 5 warm trials",
        "bare_label": "Bare ar",
        "benchmark": "c-static-library-link",
        "scenario": "Static archive, Warm",
        "language": "c",
        "samples": ARCHIVE_SAMPLES,
        # One `ar`/link call per trial.
        "hits_per_sample": 1,
    },
    "driver": {
        "header": "## C++ Driver-Link Benchmark: 50 .cpp objects, 5 warm trials",
        "bare_label": "Bare clang++",
        "benchmark": "cpp-driver-link",
        "scenario": "Driver link, Warm",
        "language": "c++",
        "samples": DRIVER_SAMPLES,
        "hits_per_sample": 1,
    },
    "rsp": {
        "header": "## Response-File Benchmark: 50 C++ files, ~500 expanded args, 5 warm trials",
        "bare_label": "Bare clang",
        "benchmark": "cpp-response-file",
        "scenario": "Multi-file RSP, Warm",
        "language": "c++",
        "samples": RSP_SAMPLES,
        # The batched row compiles NUM_FILES = 50 files per trial.
        "hits_per_sample": 50,
    },
}
BUDGET_KINDS = ("archive", "driver")
ONE_MS_NS = 1_000_000


def _fmt_dur(ns: int) -> str:
    # crates/zccache/tests/perf_bench/common.rs `fmt_dur`: whole milliseconds.
    return f"{ns / 1e9:.3f}s"


def _fmt_ratio(baseline_ns: int, ns: int) -> str:
    # common.rs `fmt_ratio`: an integer once the ratio reaches 10.
    ratio = baseline_ns / ns
    return f"**{ratio:.0f}x faster**" if ratio >= 10 else f"**{ratio:.1f}x faster**"


def _log(kind: str, bare_ns: int, sccache_ns: int, zccache_ns: int, *, metric=True) -> str:
    cfg = KINDS[kind]
    label = cfg["bare_label"]
    text = f"""
{cfg["header"]}

| Scenario | {label} | sccache | zccache | vs sccache | vs {label} |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| {cfg["scenario"]} | {_fmt_dur(bare_ns)} | {_fmt_dur(sccache_ns)} | **{_fmt_dur(zccache_ns)}** | {_fmt_ratio(sccache_ns, zccache_ns)} | {_fmt_ratio(bare_ns, zccache_ns)} |
"""
    if metric:
        record = {
            "schema_version": 1,
            "benchmark": cfg["benchmark"],
            "language": cfg["language"],
            "scenario": cfg["scenario"],
            "mode": "warm",
            "bare_duration_ns": bare_ns,
            "sccache_duration_ns": sccache_ns,
            "zccache_duration_ns": zccache_ns,
        }
        text += f"ZCCACHE_BENCH_METRIC_V1 {json.dumps(record)}\n"
    return text


def _statuses(kind: str, log: str, *, ratchet: bool = True):
    cfg = KINDS[kind]
    report = perf_guard.evaluate_attempts(
        [perf_guard.parse_attempt_log(log)],
        languages=(cfg["language"],),
        require_coverage=False,
        apply_warm_ratchet=ratchet,
    )
    return {s.baseline: s for s in report.statuses if s.scenario == cfg["scenario"]}


def _sample_id(sample) -> str:
    return f"{sample[0]}-a{sample[1]}"


def _sample_params(kinds):
    return [
        pytest.param(kind, sample, id=f"{kind}-{_sample_id(sample)}")
        for kind in kinds
        for sample in KINDS[kind]["samples"]
    ]


def _regress(kind: str, sample) -> tuple[int, int, int]:
    """The sample with a +1 ms/hit regression (#1768's size) on every hit."""
    _run, _attempt, bare, sccache, zccache = sample
    return bare, sccache, zccache + ONE_MS_NS * KINDS[kind]["hits_per_sample"]


# --- red: the pre-#1807 ratio floors flake on samples that are not regressions


def test_old_ratio_floors_rejected_unchanged_tree_samples() -> None:
    # Run 36735417112 (the reported failure, unchanged tree) and 36507856283.
    failing = 0
    for kind, sample in [(k, s) for k in KINDS for s in KINDS[k]["samples"]]:
        _run, _att, bare, _sccache, zccache = sample
        cfg = KINDS[kind]
        if bare / zccache < OLD_RATIO_FLOORS[(cfg["benchmark"], "bare")]:
            failing += 1
    assert failing >= 8  # 6 attempts of archive/driver/rsp in the two fast-class runs


# --- green: the observed noise band passes the new gate


@pytest.mark.parametrize(("kind", "sample"), _sample_params(KINDS))
def test_observed_samples_pass_the_new_gate(kind: str, sample) -> None:
    _run, _att, bare, sccache, zccache = sample
    statuses = _statuses(kind, _log(kind, bare, sccache, zccache))
    assert set(statuses) == {"bare", "sccache"}
    assert all(status.passed for status in statuses.values()), statuses


def test_reported_failure_run_now_passes_end_to_end() -> None:
    # Run 36735417112 attempt 1, all three failing rows of the C and C++ jobs
    # that this change addresses: archive link, and multi-file RSP.
    for kind in ("archive", "rsp"):
        sample = next(s for s in KINDS[kind]["samples"] if s[:2] == (36735417112, 1))
        _run, _att, bare, sccache, zccache = sample
        for baseline, status in _statuses(kind, _log(kind, bare, sccache, zccache)).items():
            assert status.passed, (kind, baseline)


def test_table_only_fallback_passes_the_observed_band() -> None:
    # Without a metric record the guard falls back to the display-rounded table
    # (`0.000s` for anything under 0.5 ms). The whole observed band is < 0.5 ms.
    for kind in BUDGET_KINDS:
        for _run, _att, bare, sccache, zccache in KINDS[kind]["samples"]:
            log = _log(kind, bare, sccache, zccache, metric=False)
            assert all(s.passed for s in _statuses(kind, log).values()), (kind, zccache)


# --- the property: an injected +1 ms/hit regression fails every changed row


@pytest.mark.parametrize(("kind", "sample"), _sample_params(KINDS))
def test_injected_1ms_per_hit_regression_fails_every_changed_row(kind: str, sample) -> None:
    bare, sccache, zccache = _regress(kind, sample)
    statuses = _statuses(kind, _log(kind, bare, sccache, zccache))
    assert set(statuses) == {"bare", "sccache"}
    assert not any(status.passed for status in statuses.values()), statuses


@pytest.mark.parametrize(("kind", "sample"), _sample_params(BUDGET_KINDS))
def test_injected_regression_fails_in_the_table_only_fallback(kind: str, sample) -> None:
    bare, sccache, zccache = _regress(kind, sample)
    log = _log(kind, bare, sccache, zccache, metric=False)
    assert not any(s.passed for s in _statuses(kind, log).values())


def test_regression_would_have_slipped_through_a_ratio_floor_on_a_slow_baseline() -> None:
    # Why a ratio is the wrong statistic here: +1 ms on a 62 ms baseline is
    # still ~40x, and a 0.9 ms hit against the same baseline is ~69x. Both are
    # far above the old floors' noise band, so only the hit time separates them.
    bare, sccache = 62_000_000, 61_000_000
    ok = _statuses("archive", _log("archive", bare, sccache, 376_000))
    slow = _statuses("archive", _log("archive", bare, sccache, 1_376_000))
    assert all(s.passed for s in ok.values())
    assert not any(s.passed for s in slow.values())
    assert bare / 1_376_000 > 40


def test_budget_boundary_uses_the_nanosecond_record() -> None:
    budget_ns = round(perf_floor.WARM_HIT_BUDGET_SECONDS[("cpp-driver-link", "Driver link, Warm")] * 1e9)
    inside = _statuses("driver", _log("driver", 60_000_000, 64_000_000, budget_ns))
    outside = _statuses("driver", _log("driver", 60_000_000, 64_000_000, budget_ns + 1))
    assert all(s.passed for s in inside.values())
    assert not any(s.passed for s in outside.values())


# --- gate definition and headroom


def test_budget_headroom_over_the_observed_max_and_below_the_1768_regression() -> None:
    for kind in BUDGET_KINDS:
        cfg = KINDS[kind]
        worst = max(sample[4] for sample in cfg["samples"]) / 1e9
        budget = perf_floor.warm_hit_budget_seconds(cfg["benchmark"], cfg["scenario"])
        assert budget is not None
        assert budget >= 2 * worst  # >= 2x over the worst hit in 15 main runs
        assert budget < 0.001  # a hit back at >= 1 ms (#1768: 1.4 ms) fails


def test_budget_rows_have_no_ratio_floor_and_every_other_row_keeps_one() -> None:
    budget_rows = {
        (benchmark, scenario, baseline)
        for benchmark, scenario in perf_floor.WARM_HIT_BUDGET_SECONDS
        for baseline in ("bare", "sccache")
    }
    assert budget_rows.isdisjoint(perf_floor.WARM_RATIO_FLOORS)
    assert len(perf_floor.WARM_RATIO_FLOORS) + len(budget_rows) == 26


def test_budget_only_applies_under_the_warm_ratchet() -> None:
    statuses = _statuses("archive", _log("archive", 38_000_000, 37_000_000, 5_000_000), ratchet=False)
    assert all(s.hit_budget_seconds is None for s in statuses.values())


def test_failure_message_names_the_budget_and_the_overshoot() -> None:
    bare, sccache, zccache = _regress("archive", ARCHIVE_SAMPLES[-3])
    report = perf_guard.evaluate_attempts(
        [perf_guard.parse_attempt_log(_log("archive", bare, sccache, zccache))],
        languages=("c",),
        require_coverage=False,
        apply_warm_ratchet=True,
    )
    message = perf_guard.format_final_status(report)
    assert "expected zccache hit <= 0.9ms" in message
    assert "actual 1.3ms" in message
    payload = perf_guard.format_report_json(report, 1.5, 1.5)
    assert {s["hit_budget_seconds"] for s in payload["statuses"] if s["mode"] == "warm"} == {0.0009}


def test_metric_precision_leaves_ratio_gated_rows_alone() -> None:
    log = _log("rsp", 4_475_970_693, 4_451_062_998, 11_489_596)
    parsed = benchmark_stats.parse_benchmark_log(log)
    assert perf_precision.apply_metric_precision(parsed, log) == parsed
    (row,) = parsed
    assert row["zccache_seconds"] == 0.011  # the table's whole-millisecond value


def test_metric_precision_ignores_malformed_records() -> None:
    log = _log("archive", 38_000_000, 37_000_000, 330_000, metric=False)
    log += "ZCCACHE_BENCH_METRIC_V1 {not json\n"
    log += 'ZCCACHE_BENCH_METRIC_V1 {"benchmark":"c-static-library-link","scenario":"Static archive, Warm","mode":"warm","zccache_duration_ns":true}\n'
    rows = perf_precision.apply_metric_precision(benchmark_stats.parse_benchmark_log(log), log)
    assert rows[0]["zccache_seconds"] != 1e-9


# --- floors that stay ratios are derived from the samples, not picked


def _guard_visible_best_ratios(kind: str, field: str) -> list[float]:
    best: dict[int, float] = {}
    for run, _att, bare, sccache, zccache in KINDS[kind]["samples"]:
        (row,) = perf_guard.parse_attempt_log(_log(kind, bare, sccache, zccache))
        best[run] = max(best.get(run, 0.0), row[field])
    return list(best.values())


@pytest.mark.parametrize(
    ("baseline", "field"),
    [("bare", "zccache_vs_bare_ratio"), ("sccache", "zccache_vs_sccache_ratio")],
)
def test_rsp_multi_file_floor_is_min_per_run_best_over_1_2(baseline: str, field: str) -> None:
    # The #1805 rule (`min / 1.2`), over the per-run best of the attempts (the
    # statistic the gate passes on), across both hosted runner classes.
    best = _guard_visible_best_ratios("rsp", field)
    assert len(best) == 15
    floor = perf_floor.warm_ratio_floor("cpp-response-file", "Multi-file RSP, Warm", baseline)
    assert floor == pytest.approx(min(best) / 1.2, abs=0.05)


def test_provenance_record_matches_the_samples() -> None:
    history = json.loads((ROOT / "ci" / "perf_threshold_history.json").read_text("utf-8"))
    (record,) = [e for e in history if e.get("kind") == "warm-floor-provenance"]
    assert record["issue"] == 1807
    by_row = {(r["benchmark"], r["scenario"]): r for r in record["rows"]}
    for cfg in KINDS.values():
        row = by_row[(cfg["benchmark"], cfg["scenario"])]
        hits = [s[4] / 1e6 for s in cfg["samples"]]
        assert row["n_attempts"] == len(hits)
        assert row["n_runs"] == len({s[0] for s in cfg["samples"]})
        assert row["run_ids"] == sorted({s[0] for s in cfg["samples"]})
        assert row["zccache_hit_ms"]["min"] == round(min(hits), 3)
        assert row["zccache_hit_ms"]["median"] == round(statistics.median(hits), 3)
        assert row["zccache_hit_ms"]["max"] == round(max(hits), 3)

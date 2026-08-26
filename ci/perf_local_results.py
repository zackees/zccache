"""Scenario execution, result validation, and rendering for perf_local."""

from __future__ import annotations

import json
import os
import re
import shutil
import statistics
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
PERF_THRESHOLDS_PATH = REPO_ROOT / "ci" / "perf_thresholds.json"
ROLLOUT_SCENARIOS = (
    "cold-tar-untar-warm",
    "worktree-share",
    "touch-no-change",
    "restore-no-clean-warm",
)
VALID_FIXTURES = ("medium", "sqlite-link")


def load_perf_thresholds() -> dict:
    """Load and validate the single source of truth for local timing gates."""
    thresholds = json.loads(PERF_THRESHOLDS_PATH.read_text(encoding="utf-8"))
    if thresholds.get("schema_version") != 1:
        raise ValueError("unsupported perf threshold manifest schema")
    warm_limits = thresholds.get("maximum_warm_ms")
    if not isinstance(warm_limits, dict) or set(warm_limits) != set(ROLLOUT_SCENARIOS):
        raise ValueError("threshold manifest must define every rollout scenario")
    if not isinstance(thresholds.get("minimum_speedup"), (int, float)):
        raise ValueError("threshold manifest minimum_speedup must be numeric")
    staged_limits = thresholds.get("maximum_staged_overhead_ms_per_publication")
    if not isinstance(staged_limits, dict) or set(staged_limits) != set(VALID_FIXTURES):
        raise ValueError("threshold manifest must define staged overhead per publication for every fixture")
    if not all(type(value) is int and value > 0 for value in staged_limits.values()):
        raise ValueError("staged overhead per-publication limits must be positive integers")
    return thresholds


PERF_THRESHOLDS = load_perf_thresholds()
LOCAL_MIN_SPEEDUP = float(PERF_THRESHOLDS["minimum_speedup"])
LOCAL_MAX_WARM_MS = PERF_THRESHOLDS["maximum_warm_ms"]
LOCAL_MAX_STAGED_OVERHEAD_MS_PER_PUBLICATION = PERF_THRESHOLDS["maximum_staged_overhead_ms_per_publication"]
LOCAL_MAX_MATERIALIZATION_COPIED_BYTES = int(PERF_THRESHOLDS["maximum_materialization_copied_bytes"])

def run_scenario(
    layout: dict[str, Path],
    scenario: str,
    fixture: str,
    jobs: int,
    results_dir: Path | None = None,
    *,
    run_command,
    host_volume_spec,
    repo_root: Path,
    image_runner: str,
) -> Path:
    """Run the per-scenario container. Returns the results dir for this run."""
    results_dir = results_dir or layout["results"] / fixture / scenario
    # Wipe last run's results so partial output from a crashing run doesn't
    # masquerade as a complete result.
    if results_dir.exists():
        remove_previous_results(
            results_dir,
            run_command=run_command,
            host_volume_spec=host_volume_spec,
            image_runner=image_runner,
        )
    results_dir.mkdir(parents=True)

    soldr_bin = layout["bin_soldr"] / "soldr"
    if not soldr_bin.is_file():
        raise FileNotFoundError(f"soldr binary missing at {soldr_bin}. Did the soldr-builder step succeed?")

    print(f"[perf-local] running scenario {scenario} x {fixture} -> {results_dir}")
    start = time.monotonic()
    # Pass any ZCCACHE_* env through to the container so the daemon's
    # env-gated instrumentation (e.g. ZCCACHE_HIT_TRACE=1 for the sub-phase
    # dump from issue #468) reaches the in-container daemon process.
    pass_through_env = [(k, v) for k, v in os.environ.items() if k.startswith("ZCCACHE_")]
    # Docker Desktop commonly has an 8 GiB VM even when the Windows host has
    # substantially more RAM. An unconstrained medium-fixture build can run
    # enough rustc processes to exhaust that VM and surface os error 12 through
    # soldr. Keep local measurements reproducible and within the selected
    # budget; callers with a larger VM can raise --jobs explicitly.
    # A performance sample must never silently switch to uncached rustc. This
    # is a benchmark-integrity requirement, not a production default.
    env_flags: list[str] = [
        "-e",
        f"CARGO_BUILD_JOBS={jobs}",
        "-e",
        "SOLDR_DAEMON_REQUIRED=1",
    ]
    for k, v in pass_through_env:
        env_flags.extend(["-e", f"{k}={v}"])
    run_command(
        [
            "docker",
            "run",
            "--rm",
            "-v",
            host_volume_spec(soldr_bin, "/usr/local/bin/soldr", "ro"),
            "-v",
            host_volume_spec(repo_root, "/zccache-src", "ro"),
            "-v",
            host_volume_spec(results_dir, "/results"),
            "-e",
            f"SCENARIO={scenario}",
            "-e",
            f"FIXTURE={fixture}",
            *env_flags,
            image_runner,
        ]
    )
    elapsed = time.monotonic() - start
    print(f"[perf-local] scenario completed in {elapsed:.1f}s")
    return results_dir


def remove_previous_results(
    results_dir: Path,
    *,
    run_command,
    host_volume_spec,
    image_runner: str,
) -> None:
    """Remove prior bind-mount output, repairing container ownership once.

    The scenario container runs as root and may create nested result
    directories with mode 0755. A normal host-side ``shutil.rmtree`` can then
    remove writable files around them but fail when it reaches a root-owned
    directory. Bind only the exact results directory into the same local
    runner image, make that tree host-removable, and retry. The container
    command is constant; destructive removal stays in Python against the
    already-resolved path.
    """
    try:
        shutil.rmtree(results_dir)
        return
    except PermissionError:
        run_command(
            [
                "docker",
                "run",
                "--rm",
                "-v",
                host_volume_spec(results_dir, "/results"),
                "--entrypoint",
                "/bin/chmod",
                image_runner,
                "-R",
                "a+rwX",
                "/results",
            ]
        )
    shutil.rmtree(results_dir)


# ---------------------------------------------------------------------------
# Result rendering for local diagnostics and retained matrix evidence.


def fmt_ms(ms) -> str:
    if ms is None or ms == "":
        return "—"
    ms = int(ms)
    if ms >= 60_000:
        return f"{ms // 60_000}m{(ms % 60_000) // 1000:02d}s"
    if ms >= 1_000:
        return f"{ms / 1000:.2f}s"
    return f"{ms}ms"


def fmt_bytes(b) -> str:
    if b is None or b == "":
        return "—"
    b = int(b)
    if b >= 1 << 30:
        return f"{b / (1 << 30):.2f} GiB"
    if b >= 1 << 20:
        return f"{b / (1 << 20):.1f} MiB"
    if b >= 1 << 10:
        return f"{b / (1 << 10):.1f} KiB"
    return f"{b} B" if b > 0 else "0 B"


def fmt_count_pct(n, total) -> str:
    if n is None or n == "":
        return "—"
    if not total:
        return str(n)
    return f"{int(n)} ({int(n) / int(total) * 100:.1f}%)"


def validate_infrastructure_result(result: dict, results_dir: Path) -> None:
    """Reject samples contaminated by soldr abort/retry behavior.

    Timing is meaningless when the build silently timed out or retried without
    the cache, so this schema and its artifact-relative evidence are validated
    before any performance threshold.
    """
    if not isinstance(result, dict):
        raise ValueError("infrastructure result must be an object")
    reasons = result.get("invalid_reasons")
    evidence = result.get("soldr_abort_evidence")
    fallback_evidence = result.get("soldr_daemon_fallback_evidence")
    valid = result.get("infrastructure_valid")
    count_names = (
        "soldr_abort_count",
        "soldr_timeout_count",
        "soldr_no_cache_retry_count",
        "soldr_daemon_fallback_count",
    )
    counts = {name: result.get(name) for name in count_names}

    malformed = (
        type(valid) is not bool
        or not isinstance(reasons, list)
        or not all(isinstance(reason, str) for reason in reasons)
        or not isinstance(evidence, list)
        or not evidence
        or not all(isinstance(item, str) and re.fullmatch(r"soldr-aborts-[A-Za-z0-9_-]+[.]jsonl", item) for item in evidence)
        or not isinstance(fallback_evidence, list)
        or not fallback_evidence
        or not all(isinstance(item, str) and re.fullmatch(r"soldr-daemon-fallbacks-[A-Za-z0-9_-]+[.]jsonl", item) for item in fallback_evidence)
        or not all(type(value) is int and value >= 0 for value in counts.values())
    )
    if malformed:
        raise ValueError("missing or malformed infrastructure-validity fields")
    if counts["soldr_timeout_count"] > counts["soldr_abort_count"]:
        raise ValueError("timeout count exceeds abort count")
    if counts["soldr_no_cache_retry_count"] > counts["soldr_abort_count"]:
        raise ValueError("no-cache retry count exceeds abort count")
    if valid != (len(reasons) == 0):
        raise ValueError("infrastructure validity and reasons disagree")
    missing = [item for item in evidence if not (results_dir / item).is_file()]
    if missing:
        raise ValueError(f"declared soldr abort evidence is missing: {missing[0]}")
    missing_fallback = [item for item in fallback_evidence if not (results_dir / item).is_file()]
    if missing_fallback:
        raise ValueError(f"declared soldr daemon fallback evidence is missing: {missing_fallback[0]}")
    if not valid or any(counts.values()):
        detail = "; ".join(reasons) or "soldr abort detected"
        raise ValueError(
            f"contaminated benchmark sample: {detail}; aborts={counts['soldr_abort_count']}, timeouts={counts['soldr_timeout_count']}, no-cache retries={counts['soldr_no_cache_retry_count']}, daemon fallbacks={counts['soldr_daemon_fallback_count']}"
        )


def _read_session_report(results_dir: Path, names: tuple[str, ...]) -> dict | None:
    for name in names:
        path = results_dir / name
        if not path.is_file():
            continue
        try:
            payload = json.loads(path.read_text())
        except json.JSONDecodeError:
            return None
        session = payload.get("last_session")
        return session if isinstance(session, dict) else None
    return None


def _staged_profile(report: dict | None) -> dict | None:
    if not report:
        return None
    profile = report.get("phase_profile")
    if not isinstance(profile, dict):
        return None
    staged = profile.get("staged")
    return staged if isinstance(staged, dict) else None


def evaluate_rollout_result(results_dir: Path, scenario: str, fixture: str) -> list[str]:
    """Return every hard-gate failure for one sanctioned local matrix cell."""
    failures: list[str] = []
    result_path = results_dir / "result.json"
    if not result_path.is_file():
        return ["result.json missing"]
    try:
        result = json.loads(result_path.read_text())
    except json.JSONDecodeError as error:
        return [f"result.json is malformed: {error}"]
    if not isinstance(result, dict):
        return ["result.json must contain one object"]

    try:
        validate_infrastructure_result(result, results_dir)
    except ValueError as error:
        failures.append(str(error))

    cold_key = "a_ms" if scenario == "worktree-share" else "cold_ms"
    warm_key = "b_ms" if scenario == "worktree-share" else "warm_ms"
    cold_ms = result.get(cold_key)
    warm_ms = result.get(warm_key)
    if type(cold_ms) is not int or type(warm_ms) is not int or warm_ms <= 0:
        failures.append(f"invalid timing fields {cold_key}={cold_ms} {warm_key}={warm_ms}")
    else:
        speedup = cold_ms / warm_ms
        if speedup < LOCAL_MIN_SPEEDUP:
            failures.append(f"speedup {speedup:.2f}x is below {LOCAL_MIN_SPEEDUP:.2f}x")
        warm_limit = LOCAL_MAX_WARM_MS[scenario]
        if warm_limit is not None and warm_ms > warm_limit:
            failures.append(f"warm time {warm_ms}ms exceeds {warm_limit}ms")

    cold_report = _read_session_report(results_dir, ("cold-cache-report.json", "a-cache-report.json"))
    warm_report = _read_session_report(results_dir, ("warm-cache-report.json", "b-cache-report.json"))
    cold_staged = _staged_profile(cold_report)
    warm_staged = _staged_profile(warm_report)
    if cold_staged is None:
        failures.append("missing cold staged telemetry")
        return failures

    cold_timings = cold_staged.get("timings_ns", {})
    cold_counters = cold_staged.get("counters", {})
    if not isinstance(cold_timings, dict) or not isinstance(cold_counters, dict):
        failures.append("malformed cold staged telemetry")
        return failures
    overhead_ns = sum(int(cold_timings.get(name, 0) or 0) for name in ("hashing", "publication", "miss_materialization"))
    publications = int(cold_counters.get("publication_success", 0) or 0)
    if publications <= 0:
        failures.append("cold path published no staged generations")
    else:
        # These timings accumulate independently across concurrent compiler
        # requests. Normalize their sum so an identical per-publication cost
        # receives the same verdict in a four-crate and a 143-crate fixture.
        overhead_ms_per_publication = (overhead_ns + publications * 1_000_000 - 1) // (publications * 1_000_000)
        overhead_limit = int(LOCAL_MAX_STAGED_OVERHEAD_MS_PER_PUBLICATION[fixture])
        if overhead_ms_per_publication > overhead_limit:
            failures.append("staged miss overhead " f"{overhead_ms_per_publication}ms/publication exceeds " f"{overhead_limit}ms/publication for {fixture}")

    counter_sets = [cold_counters]
    if warm_staged is not None:
        warm_counters = warm_staged.get("counters", {})
        warm_bytes = warm_staged.get("bytes", {})
        if not isinstance(warm_counters, dict) or not isinstance(warm_bytes, dict):
            failures.append("malformed warm staged telemetry")
            return failures
        counter_sets.append(warm_counters)
        copied = int(warm_bytes.get("materialization_copied", 0) or 0)
        tiers = sum(
            int(warm_counters.get(name, 0) or 0)
            for name in (
                "materialize_reflink",
                "materialize_hardlink_shared",
                "materialize_copy",
            )
        )
    elif scenario == "restore-no-clean-warm":
        copied = 0
        tiers = 0
    else:
        failures.append("missing warm staged telemetry")
        return failures

    salvage = sum(int(counters.get("salvage_attempt", 0) or 0) for counters in counter_sets)
    critical = sum(int(counters.get(name, 0) or 0) for counters in counter_sets for name in ("publication_failure", "publication_conflict", "materialize_failure"))
    if salvage != 0 or critical != 0:
        failures.append(f"salvage={salvage} critical_failures={critical}")
    if copied > LOCAL_MAX_MATERIALIZATION_COPIED_BYTES:
        failures.append(f"materialization copied {copied} bytes, max {LOCAL_MAX_MATERIALIZATION_COPIED_BYTES}")
    if scenario == "restore-no-clean-warm":
        if result.get("warm_misses") != 0:
            failures.append(f"restore warm build had cache misses: {result.get('warm_misses')}")
    elif tiers <= 0:
        failures.append("warm build reported no materialization tier")

    return failures


def render_summary(results_dir: Path, scenario: str, fixture: str) -> int:
    """Print a one-row summary table + the inline annotation that the GHA
    Evaluate step would emit. Returns 0 if the speedup hit the 3x gate."""
    result_json = results_dir / "result.json"
    if not result_json.is_file():
        print(f"[perf-local] FAIL: result.json missing at {result_json}")
        return 1
    result = json.loads(result_json.read_text())

    # Per-scenario key naming, matches Evaluate's cold_key_for/warm_key_for.
    cold_key = "a_ms" if scenario == "worktree-share" else "cold_ms"
    warm_key = "b_ms" if scenario == "worktree-share" else "warm_ms"
    cold_ms = result.get(cold_key)
    warm_ms = result.get(warm_key)
    if cold_ms is None or warm_ms is None or warm_ms <= 0:
        print(f"[perf-local] FAIL: bad timing in result.json (cold={cold_ms} warm={warm_ms})")
        return 1
    speedup = cold_ms / warm_ms

    # Warm-side cache report carries the rich session counters.
    report_candidates = [
        results_dir / "warm-cache-report.json",
        results_dir / "b-cache-report.json",
    ]
    report = None
    for candidate in report_candidates:
        if candidate.is_file():
            report = json.loads(candidate.read_text()).get("last_session", {})
            break
    if report is None:
        report = {}

    # `last-session-stats.json` is zccache's own JSON output (written by
    # `zccache session-end --json`); it includes `phase_profile` from
    # PROTOCOL_VERSION 9 onward. Soldr's `cache report` is the
    # canonical structured form but it copies a fixed set of keys into
    # `last_session` and strips unknown fields, so a fresh phase_profile
    # field arrives in `last-session-stats.json` before it surfaces in
    # the report block. Pull it directly to avoid that lag.
    if "phase_profile" not in report:
        stats_candidates = [
            results_dir / "warm-zccache-logs" / "last-session-stats.json",
            results_dir / "b-zccache-logs" / "last-session-stats.json",
        ]
        for candidate in stats_candidates:
            if not candidate.is_file():
                continue
            try:
                raw = json.loads(candidate.read_text())
            except json.JSONDecodeError:
                continue
            phase = raw.get("phase_profile")
            if phase is not None:
                report["phase_profile"] = phase
                break

    compiles = report.get("compilations")
    hits = report.get("hits")
    misses = report.get("misses")
    non_cache = report.get("non_cacheable")
    errs = report.get("errors")
    bytes_w = report.get("bytes_written")
    time_saved = report.get("time_saved_ms")
    unique_srcs = report.get("unique_sources")
    daemon_rss = result.get("peak_daemon_rss_bytes")
    compile_rss = result.get("peak_compile_rss_bytes")

    threshold = LOCAL_MIN_SPEEDUP
    verdict = "PASS" if speedup >= threshold else "FAIL"

    print()
    print(f"## Perf result — local Docker harness — {fixture} / {scenario}")
    print()
    header = "| Fixture | Scenario | Verdict | Speedup | Need | Cold | Warm | Compiles | Hits | Misses | Ignored | Errors | Unique Srcs | Bytes W | Time Saved | Daemon RSS | Compile RSS |"
    sep = "| --- | --- | :---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    row = (
        f"| {fixture} | {scenario} | **{verdict}** | {speedup:.2f}x | >={threshold:.2f}x "
        f"| {fmt_ms(cold_ms)} | {fmt_ms(warm_ms)} "
        f"| {compiles if compiles is not None else '—'} "
        f"| {fmt_count_pct(hits, compiles)} "
        f"| {fmt_count_pct(misses, compiles)} "
        f"| {fmt_count_pct(non_cache, compiles)} "
        f"| {errs if errs is not None else '—'} "
        f"| {unique_srcs if unique_srcs is not None else '—'} "
        f"| {fmt_bytes(bytes_w)} | {fmt_ms(time_saved)} "
        f"| {fmt_bytes(daemon_rss)} | {fmt_bytes(compile_rss)} |"
    )
    print(header)
    print(sep)
    print(row)
    print()
    print(
        f"{fixture}/{scenario}: speedup={speedup:.2f}x (need >={threshold:.2f}x); "
        f"cold={fmt_ms(cold_ms)} warm={fmt_ms(warm_ms)}; "
        f"compiles={compiles or 0} hits={hits or 0} misses={misses or 0} "
        f"ignored={non_cache or 0} errors={errs or 0}; "
        f"bytes_W={fmt_bytes(bytes_w)} daemon_RSS={fmt_bytes(daemon_rss)}"
    )

    render_phase_breakdown(report.get("phase_profile"))

    return 0 if verdict == "PASS" else 1


def render_phase_breakdown(phase_profile) -> None:
    """Print a phase-breakdown table from `SessionStats.phase_profile`.

    Skipped silently when the daemon didn't populate the field (old
    PROTOCOL_VERSION) or when both hit and miss counts are zero.
    """
    if not isinstance(phase_profile, dict):
        return
    hit_count = int(phase_profile.get("hit_count") or 0)
    miss_count = int(phase_profile.get("miss_count") or 0)
    if hit_count == 0 and miss_count == 0:
        return

    # (label, total-ns, denom-count). Hit phases use hit_count for the
    # per-event average; miss phases use miss_count. The two metadata-cache
    # sub-phases are summed so the table speaks the language used in design
    # discussion ("metadata cache (source+hdrs)").
    src_ns = int(phase_profile.get("hash_source_ns") or 0)
    hdr_ns = int(phase_profile.get("hash_headers_ns") or 0)
    rows = [
        ("parse_args", int(phase_profile.get("parse_args_ns") or 0), hit_count),
        ("build_context", int(phase_profile.get("build_context_ns") or 0), hit_count),
        ("metadata cache (source+hdrs)", src_ns + hdr_ns, hit_count),
        ("depgraph_check", int(phase_profile.get("depgraph_check_ns") or 0), hit_count),
        (
            "request_cache_lookup",
            int(phase_profile.get("request_cache_lookup_ns") or 0),
            hit_count,
        ),
        (
            "cross_root_validate",
            int(phase_profile.get("cross_root_validate_ns") or 0),
            hit_count,
        ),
        (
            "artifact_lookup",
            int(phase_profile.get("artifact_lookup_ns") or 0),
            hit_count,
        ),
        (
            "write_output (materialize)",
            int(phase_profile.get("write_output_ns") or 0),
            hit_count,
        ),
        ("bookkeeping", int(phase_profile.get("bookkeeping_ns") or 0), hit_count),
        ("compiler_exec", int(phase_profile.get("compiler_exec_ns") or 0), miss_count),
        ("include_scan", int(phase_profile.get("include_scan_ns") or 0), miss_count),
        ("hash_all", int(phase_profile.get("hash_all_ns") or 0), miss_count),
        (
            "artifact_store",
            int(phase_profile.get("artifact_store_ns") or 0),
            miss_count,
        ),
    ]
    rows.sort(key=lambda r: r[1], reverse=True)

    print()
    print(f"### Phase breakdown (warm-side daemon — {hit_count} hits, {miss_count} misses)")
    print()
    print("| Phase | Total ms | Avg per event (µs) |")
    print("| --- | ---: | ---: |")
    for label, total_ns, denom in rows:
        if total_ns == 0:
            continue
        total_ms = total_ns / 1_000_000
        if denom > 0:
            avg_us = total_ns / denom / 1_000
            avg_cell = f"{avg_us:.1f}"
        else:
            avg_cell = "—"
        print(f"| {label} | {total_ms:.1f} | {avg_cell} |")

    total_hit_ns = int(phase_profile.get("total_hit_ns") or 0)
    total_miss_ns = int(phase_profile.get("total_miss_ns") or 0)
    print()
    print(f"total_hit_ns={total_hit_ns / 1_000_000:.1f}ms total_miss_ns={total_miss_ns / 1_000_000:.1f}ms")


# ---------------------------------------------------------------------------



def _shell_quote(arg: str) -> str:
    """Minimal quoting for embedding an argv element inside a `bash -c`
    string. Wraps in single quotes and escapes any embedded single
    quotes — exactly what `shlex.quote` produces, inlined here so we
    don't pull in `shlex` for this one call site."""
    if arg and all(c.isalnum() or c in "@%+=:,./-_" for c in arg):
        return arg
    return "'" + arg.replace("'", "'\"'\"'") + "'"


def _distribution(values: list[int]) -> dict[str, float | int]:
    """Return stable summary statistics for repeated timing samples."""
    ordered = sorted(values)
    median = statistics.median(ordered)
    deviations = [abs(value - median) for value in ordered]
    return {
        "count": len(ordered),
        "min_ms": ordered[0],
        "median_ms": median,
        "p95_ms": ordered[max(0, (len(ordered) * 95 + 99) // 100 - 1)],
        "mad_ms": statistics.median(deviations),
        "max_ms": ordered[-1],
    }


def _write_repeat_summary(
    base_dir: Path,
    samples: list[tuple[Path, dict]],
    scenario: str,
    fixture: str,
) -> None:
    timings: dict[str, list[int]] = {"cold_ms": [], "warm_ms": []}
    for sample_dir, result in samples:
        cold_key = "a_ms" if scenario == "worktree-share" else "cold_ms"
        warm_key = "b_ms" if scenario == "worktree-share" else "warm_ms"
        timings["cold_ms"].append(int(result[cold_key]))
        timings["warm_ms"].append(int(result[warm_key]))
    summary = {
        "schema_version": 1,
        "fixture": fixture,
        "scenario": scenario,
        "samples": [str(path.relative_to(base_dir)) for path, _ in samples],
        "distributions": {name: _distribution(values) for name, values in timings.items()},
    }
    (base_dir / "repeat-summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")

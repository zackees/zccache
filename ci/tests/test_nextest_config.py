import re
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def _exclusive_pattern() -> re.Pattern[str]:
    config = tomllib.loads((ROOT / ".config" / "nextest.toml").read_text(encoding="utf-8"))
    for override in config["profile"]["default"]["overrides"]:
        if override.get("threads-required") == "num-cpus":
            match = re.fullmatch(r"test\(/(.*)/\)", override["filter"])
            assert match, override["filter"]
            return re.compile(match.group(1))
    raise AssertionError("no exclusive (num-cpus) override in .config/nextest.toml")


def test_timing_sensitive_tests_run_alone_under_nextest() -> None:
    """Wall-clock assertions must not share the CPU with parallel neighbours."""
    pattern = _exclusive_pattern()
    for name in (
        "cli_wrapper_startup_budget::development_namespace_is_not_rehashed_per_invocation",
        "daemon_idle_cpu_budget_test::idle_daemon_stays_within_cpu_budget",
        "perf_c_zccache_vs_bare",
        "miss_overhead_budget_depfile",
    ):
        assert pattern.search(name), name
    assert not pattern.search("daemon_rustc_restore_test::first_compile_waits_for_background_depgraph_load")

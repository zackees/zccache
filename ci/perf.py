"""Run performance benchmarks (zccache vs sccache vs bare compiler).

Four benchmarks:
  - perf_c_zccache_vs_bare: C inline args
  - perf_warm_cache_zccache_vs_sccache: C++ inline args (single-file + multi-file)
  - perf_response_file: C++ large nested response files (~283 expanded args)
  - perf_rustc_zccache_vs_sccache: Rust compilation (50 .rs lib files)

Usage:
    ./perf.sh               # run all benchmarks
    ./perf.sh --nocapture   # (default) show output as it runs
"""

import subprocess
import sys
from pathlib import Path

from ci.soldr import cargo_command, self_build_env

SCRIPT_DIR = Path(__file__).parent.parent.resolve()


def main():
    cmd = cargo_command(
        "test",
        "-p", "zccache",
        "--test", "daemon_perf_bench_test",
        "--",
        "--nocapture",
        "--ignored",
        "--test-threads=1",
    )

    result = subprocess.run(
        cmd,
        text=True,
        encoding="utf-8",
        errors="replace",
        cwd=str(SCRIPT_DIR),
        env=self_build_env(),
    )
    return result.returncode


if __name__ == "__main__":
    sys.exit(main())

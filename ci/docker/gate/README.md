# ci/docker/gate

The isolated environment for the local gate's `tests` lane
(`local-gate.toml`, zackees/ci.yml#168 GATE-005 and #196 GATE-009).

zccache is live infrastructure on a developer host: soldr routes every
compile through its daemon. The test suite starts zccache daemons and
touches zccache state roots, so it never runs on the host. The bosn task
`gate-test` (`bosn.toml`) builds `Dockerfile` here and runs
`run_gate_tests.sh` with the checkout mounted at `/work`.

- `Dockerfile` - Ubuntu 24.04 (the hosted Integration runner's OS) with
  clang, CMake/Ninja, Python 3 and a pinned soldr release. It sets
  `ZCCACHE_TEST_ISOLATED=1`, the marker `ci/nextest_isolation_guard.sh`
  accepts in place of `CI=true`, and runs as the unprivileged user `gate`
  (like the hosted runner): as root, read-only cache files stay writable and
  the strict artifact-layout validation fails.
- `run_gate_tests.sh` - echoes the gate's `.gate-nonce` (proof the container
  saw this worktree), then runs exactly the test commands of the
  Integration (Linux) job and the MSRV job's nested Dylint cache contract.

Run it through the gate (`ci/local_gate.py --lane tests`) or directly with
`bosn run --task gate-test`.

# zccache-daemon-core integration tests

Ignored-by-default integration tests that start real daemon instances and
invoke the host toolchain (clang, ar). Run on Linux via `./test --integration`.

- `legacy_path_validation.rs` — aggregate strict cache-layout validation for
  issue #1152: exercises single/multi compile, link, generic exec, and daemon
  restart flows, then asserts no non-migration legacy artifact-path access was
  logged and the cache-log audit passes.

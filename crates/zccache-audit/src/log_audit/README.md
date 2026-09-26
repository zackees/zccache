# log_audit submodules

Child modules of `log_audit.rs`.

- `source_fixture_tests.rs` — #1523 contract test. Pins the files the audit
  reads to `ci/log_audit_source_fixture.json`, the fixture the Integration
  workflow's `ci/clear_runtime_telemetry.py` cleanup is also tested against.

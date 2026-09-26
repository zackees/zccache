# `zccache.fingerprint`

Python API over the Rust fingerprinting engine — content hashes and change
detection for a file set, without shelling out to the `zccache-fp` binary.

`__init__.py` is the public surface (`Api`, `FingerprintCache`,
`FingerprintDecision`, `FingerprintManager`, `FingerprintResult`); the
underscore modules are implementation detail and may change without notice.

- `_manager.py` — `FingerprintManager`: `read` / `write` / `check` /
  `mark_ran` / `save` / `save_many` / `save_all`, plus an mtime fast path
  that avoids hashing when the cheap stat already proves nothing changed.
  `check` only inspects; completion must be recorded per operation, and
  `save_all` never certifies a checked operation that did not run (#1650).
  See the crate README's "Checking is not completing" section.
  This file is byte-identical to
  `crates/zccache-fingerprint/python/zccache/fingerprint/_manager.py`, which
  carries the tests; `ci/tests/test_fingerprint_manager_parity.py` enforces it.
- `_result.py` — `FingerprintResult`, the dataclass returned to callers.

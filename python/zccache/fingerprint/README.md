# `zccache.fingerprint`

Python API over the Rust fingerprinting engine — content hashes and change
detection for a file set, without shelling out to the `zccache-fp` binary.

`__init__.py` is the public surface (`Api`, `FingerprintCache`,
`FingerprintDecision`, `FingerprintManager`, `FingerprintResult`); the
underscore modules are implementation detail and may change without notice.

- `_manager.py` — `FingerprintManager`: `read` / `write` / `check` /
  `save_all`, plus an mtime fast path that avoids hashing when the cheap
  stat already proves nothing changed.
- `_result.py` — `FingerprintResult`, the dataclass returned to callers.

# `python/tests/cpp_lint/`

Tests for the `zccache.cpp_lint` Python API (issue #841).

Two layers:

| Test file                       | What it covers                                                | Tool deps                                          |
|---------------------------------|---------------------------------------------------------------|----------------------------------------------------|
| `test_types.py`                 | Dataclass shapes, default values, frozen-ness, `Summary` rendering. | none                                               |
| `test_validation.py`            | `validate()` errors for every documented bad shape.           | none                                               |
| `test_toml.py`                  | TOML dump → parse round-trip + stdlib fallback writer.        | none                                               |
| `test_listorpath.py`            | `resolve_to_lines` for every variant (string, tuple, file).   | none                                               |
| `test_cache.py`                 | `LintCache` put/get/round-trip; deterministic-vs-transient policy. | none                                               |
| `test_tools_resolution.py`      | tool path resolution + `MissingClangPolicy` modes (with fakes). | none                                               |
| `test_runner_no_tools.py`       | `cpp_lint()` end-to-end with **fake** clang-query/IWYU subprocesses, exercises filter / abort / max_errors / order. | none (uses stub executables under tmp)             |
| `test_clang_query_integration.py` | Real clang-query against a tiny TU.                         | `clang-tool-chain-bins` (auto-skip if unavailable) |
| `test_iwyu_integration.py`      | Real IWYU against a tiny TU.                                  | `clang-tool-chain-bins` (auto-skip if unavailable) |

## Running

```bash
pytest python/tests/cpp_lint -v
```

Stdlib-only tests run on every platform. Integration tests
auto-`pytest.skip` when the relevant tool can't be resolved (no
network, fetch failure, or the binary just isn't available for the
host platform).

## Fixtures

- `tmp_compile_commands(tmp_path, tu)` — writes a minimal
  `compile_commands.json` pointing at `tu` with default flags.
- `stub_clang_query(tmp_path)` — drops a tiny shell script that
  mimics clang-query's diag output, used by `test_runner_no_tools.py`.
- `stub_iwyu(tmp_path)` — same idea for IWYU.

See `conftest.py`.

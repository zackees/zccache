# `zccache.cpp_lint`

Python API for caching C/C++ lint tools — `clang-query` AST matchers
and `include-what-you-use` (IWYU) today, `cppcheck` in the future.
Implements the spec from
[issue #841](https://github.com/zackees/zccache/issues/841).

## Quick start

```python
from pathlib import Path
from zccache.cpp_lint import (
    cpp_lint, LintInput, AstQuery, IwyuItem,
    MissingClangPolicy, ResultFilter, Summary,
)

NOEXCEPT = AstQuery(
    name="noexcept",
    matcher_body=Path("ci/queries/noexcept.cqs"),
    cache_key_namespace=b"v2",
)

lint_input = LintInput(
    compile_commands=Path(".cache/compile_commands.json"),
    ast_queries=(NOEXCEPT,),
    default_scope=Path("ci/scopes/fl_lint_scope.txt"),
    default_ignore=("**/third_party/**",),
    allow_missing_clang=MissingClangPolicy.FETCH,  # auto-install via clang-tool-chain-bins
    max_errors=10,                                  # abort early once 10 errors land
)

for item in cpp_lint(lint_input):
    if isinstance(item, Summary):
        print(item.to_str())
    elif item.error:
        print(f"ERR  [{item.item_name}] {item.path}: {item.message}")
    elif item.warning:
        print(f"WARN [{item.item_name}] {item.path}:{item.line} {item.message}")
```

## Layout

| File           | Purpose                                                            |
|----------------|--------------------------------------------------------------------|
| `__init__.py`  | Re-exports the public API.                                         |
| `_types.py`    | Frozen dataclasses + enums (`LintInput`, `AstQuery`, `IwyuItem`, `ResultItem`, `Summary`, `ResultKind`, `CacheStatus`, `ResultFilter`, `MissingClangPolicy`). |
| `_validate.py` | Upfront `LintInput` validation; raises `LintInputError`.           |
| `_toml.py`     | Round-trippable TOML serialization of `LintInput`.                 |
| `_tools.py`    | Tool path resolution (`None` → env PATH; `FETCH` → `clang_tool_chain_bins.ensure`). |
| `_listorpath.py` | Resolution of the polymorphic `ListOrPath` scope/ignore type.   |
| `_cache.py`    | Per-(TU, item) on-disk JSON cache; blake3 keys.                    |
| `_clang_query.py` | `clang-query` subprocess driver + output parser.                |
| `_iwyu.py`     | IWYU subprocess driver + output parser; optional `fix_includes.py` auto-fix. |
| `_runner.py`   | `cpp_lint(LintInput)` generator entry point; thread pool dispatch. |

## Hooks vs gates (this directory)

This is a pure-Python module that implements the API surface from #841
end-to-end. It has its own test suite under `python/tests/cpp_lint/`.

The current implementation runs `clang-query` and IWYU as subprocesses
on the calling Python thread pool. The follow-up daemon integration
described in #841 (GIL-isolated pyo3 feeder, depgraph-walk oracle,
JSONL event log) lands incrementally — the dataclass surface here is
the long-term contract; the dispatcher behind it evolves.

## Tool resolution

When the relevant tool path field on `LintInput` is `None`:

1. `shutil.which("clang-query")` (or `include-what-you-use`, `fix_includes.py`)
2. If still missing AND `allow_missing_clang=MissingClangPolicy.FETCH`,
   try `clang_tool_chain_bins.ensure(...)` — names of fetched tools
   land in `Summary.tools_fetched`.
3. Otherwise raise `RuntimeError` listing what's missing.

The final resolved paths used by the run land in
`Summary.resolved_tool_paths` regardless of how they were resolved.

## Cache

Per-(TU, item) on-disk JSON cache keyed by blake3 of:
- TU path + content hash
- Item-specific config (matcher body, mapping files, extra args)
- `cache_key_namespace`
- Compile flags hash

Deterministic failures (`PARSE_ERROR`, `MATCHER_SYNTAX`,
`IWYU_CONFIG`, `COMPILE_FLAGS`) are cached. Transient failures
(`TIMEOUT`, `OOM`, `SIGNAL`, `INTERNAL`) are not.

Set `cached=False` on `cpp_lint(...)` to skip cache reads (still
writes); useful for verifying fresh tool output.

## Abort + max_errors

Two ways to terminate early:

- `LintInput.abort_signal: threading.Event | None` — set the event
  from any thread; the dispatcher stops scheduling new jobs and drains
  in-flight ones to natural completion, then emits `Summary(aborted=True)`.
- `LintInput.max_errors: int | None` — once that many `ResultItem`s
  with `error=True` have been streamed, the dispatcher triggers the
  same drain-and-abort path. Useful for CI smoke runs that don't need
  to see every failure.

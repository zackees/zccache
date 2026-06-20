"""TOML serialization for LintInput.

Direction: Python → TOML string. The TOML form is what crosses the
FFI boundary into the daemon in the eventual pyo3 integration; until
then it doubles as a stable round-trip representation that callers can
inspect or hash.

For symmetry, `load_lint_input_toml(text)` reconstructs a LintInput
from a TOML string written by `dump_lint_input_toml(...)`.

`threading.Event`, callables, and other non-serializable fields are
omitted from the TOML output — only stable configuration travels
across the boundary.
"""

from __future__ import annotations

import sys
from pathlib import Path
from typing import Any

from zccache.cpp_lint._types import (
    AstQuery,
    IwyuItem,
    LintInput,
    MissingClangPolicy,
)

if sys.version_info >= (3, 11):
    import tomllib as _tomllib  # type: ignore[import-not-found]
else:  # pragma: no cover
    import tomli as _tomllib  # type: ignore[import-not-found,assignment]

try:
    import tomli_w  # type: ignore[import-not-found]

    _HAS_TOMLI_W = True
except ImportError:  # pragma: no cover
    tomli_w = None  # type: ignore[assignment]
    _HAS_TOMLI_W = False


def dump_lint_input_toml(lint_input: LintInput) -> str:
    """Serialize a LintInput to a TOML string.

    `abort_signal` (a threading.Event) is intentionally omitted.
    """
    payload = _lint_input_to_payload(lint_input)
    return _to_toml_str(payload)


def load_lint_input_toml(text: str) -> LintInput:
    """Deserialize a LintInput from a TOML string.

    The reverse of `dump_lint_input_toml`. `abort_signal` always reads
    back as None (omitted from TOML), `max_jobs`/`cache_root` defaults
    apply when absent.
    """
    data = _tomllib.loads(text)
    return _payload_to_lint_input(data)


def _lint_input_to_payload(lint_input: LintInput) -> dict[str, Any]:
    payload: dict[str, Any] = {}
    payload["compile_commands"] = str(lint_input.compile_commands)

    payload["default_scope"] = _scope_to_payload(lint_input.default_scope)
    payload["default_ignore"] = _scope_to_payload(lint_input.default_ignore)
    payload["let_bindings"] = _scope_to_payload(lint_input.let_bindings)
    payload["extra_clang_query_args"] = list(lint_input.extra_clang_query_args)
    payload["default_mapping_files"] = [str(p) for p in lint_input.default_mapping_files]
    payload["extra_iwyu_args"] = list(lint_input.extra_iwyu_args)

    if lint_input.clang_query_path is not None:
        payload["clang_query_path"] = str(lint_input.clang_query_path)
    if lint_input.iwyu_path is not None:
        payload["iwyu_path"] = str(lint_input.iwyu_path)
    if lint_input.fix_includes_path is not None:
        payload["fix_includes_path"] = str(lint_input.fix_includes_path)

    payload["allow_missing_clang"] = lint_input.allow_missing_clang.value

    if lint_input.max_jobs is not None:
        payload["max_jobs"] = lint_input.max_jobs
    if lint_input.cache_root is not None:
        payload["cache_root"] = str(lint_input.cache_root)
    if lint_input.max_errors is not None:
        payload["max_errors"] = lint_input.max_errors

    if lint_input.ast_queries:
        payload["ast_queries"] = [
            _ast_query_to_payload(q) for q in lint_input.ast_queries
        ]
    if lint_input.iwyu_items:
        payload["iwyu_items"] = [
            _iwyu_item_to_payload(r) for r in lint_input.iwyu_items
        ]

    return payload


def _ast_query_to_payload(q: AstQuery) -> dict[str, Any]:
    payload: dict[str, Any] = {
        "name": q.name,
        "matcher_body": str(q.matcher_body),
    }
    if q.scope is not None:
        payload["scope"] = _scope_to_payload(q.scope)
    if q.ignore is not None:
        payload["ignore"] = _scope_to_payload(q.ignore)
    if q.cache_key_namespace:
        payload["cache_key_namespace"] = _bytes_to_str(q.cache_key_namespace)
    return payload


def _iwyu_item_to_payload(r: IwyuItem) -> dict[str, Any]:
    payload: dict[str, Any] = {
        "name": r.name,
        "mapping_files": [str(p) for p in r.mapping_files],
        "pch_in_code": r.pch_in_code,
        "extra_args": list(r.extra_args),
        "auto_fix": r.auto_fix,
    }
    if r.scope is not None:
        payload["scope"] = _scope_to_payload(r.scope)
    if r.ignore is not None:
        payload["ignore"] = _scope_to_payload(r.ignore)
    if r.cache_key_namespace:
        payload["cache_key_namespace"] = _bytes_to_str(r.cache_key_namespace)
    return payload


def _scope_to_payload(scope: object) -> Any:
    if scope is None:
        return []
    if isinstance(scope, str):
        return scope
    if isinstance(scope, Path):
        return str(scope)
    if isinstance(scope, tuple):
        return [str(s) for s in scope]
    raise TypeError(f"unsupported scope payload type: {type(scope).__name__}")


def _bytes_to_str(value: bytes) -> str:
    try:
        return value.decode("utf-8")
    except UnicodeDecodeError:
        return value.hex()


def _to_toml_str(payload: dict[str, Any]) -> str:
    if _HAS_TOMLI_W and tomli_w is not None:
        return tomli_w.dumps(payload)
    return _stdlib_toml_dumps(payload)


def _stdlib_toml_dumps(payload: dict[str, Any]) -> str:
    """Minimal TOML writer covering the shape `dump_lint_input_toml` emits.

    Limited to the constructs we actually use:
      - top-level scalars (str, int, float, bool)
      - top-level homogeneous lists of strings or scalars
      - top-level mixed lists rendered inline (strings always quoted)
      - arrays of tables under `[[name]]` headers

    No nested dicts under top-level scalars; no datetime; no comments.
    Good enough for the LintInput shape and avoids a hard third-party
    dep on tomli_w.
    """
    out: list[str] = []
    array_blocks: list[tuple[str, list[dict[str, Any]]]] = []
    for key, value in payload.items():
        if isinstance(value, list) and value and isinstance(value[0], dict):
            array_blocks.append((key, value))
            continue
        out.append(f"{_toml_key(key)} = {_toml_value(value)}")
    for name, items in array_blocks:
        for item in items:
            out.append("")
            out.append(f"[[{_toml_key(name)}]]")
            for k, v in item.items():
                out.append(f"{_toml_key(k)} = {_toml_value(v)}")
    return "\n".join(out) + ("\n" if out else "")


def _toml_key(key: str) -> str:
    # Bare keys: ASCII letters, digits, underscore, dash. Otherwise quote.
    if key and all(c.isalnum() or c in "_-" for c in key):
        return key
    return _quote_string(key)


def _toml_value(value: Any) -> str:
    if value is True:
        return "true"
    if value is False:
        return "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        return repr(value)
    if isinstance(value, str):
        return _quote_string(value)
    if isinstance(value, list):
        return "[" + ", ".join(_toml_value(v) for v in value) + "]"
    raise TypeError(f"unsupported TOML value type: {type(value).__name__}")


def _quote_string(s: str) -> str:
    # TOML basic string: ASCII control chars escaped, backslash and
    # double-quote escaped. Newlines as \n.
    out = ['"']
    for ch in s:
        if ch == "\\":
            out.append("\\\\")
        elif ch == '"':
            out.append('\\"')
        elif ch == "\n":
            out.append("\\n")
        elif ch == "\r":
            out.append("\\r")
        elif ch == "\t":
            out.append("\\t")
        elif ord(ch) < 0x20:
            out.append(f"\\u{ord(ch):04x}")
        else:
            out.append(ch)
    out.append('"')
    return "".join(out)


def _payload_to_lint_input(data: dict[str, Any]) -> LintInput:
    ast_queries = tuple(
        _payload_to_ast_query(q) for q in data.get("ast_queries", [])
    )
    iwyu_items = tuple(
        _payload_to_iwyu_item(r) for r in data.get("iwyu_items", [])
    )

    policy = MissingClangPolicy(data.get("allow_missing_clang", "error"))

    return LintInput(
        compile_commands=Path(data["compile_commands"]),
        ast_queries=ast_queries,
        iwyu_items=iwyu_items,
        default_scope=_payload_to_scope(data.get("default_scope")),
        default_ignore=_payload_to_scope(data.get("default_ignore")),
        let_bindings=_payload_to_scope(data.get("let_bindings")) or (),
        extra_clang_query_args=tuple(data.get("extra_clang_query_args", [])),
        default_mapping_files=tuple(
            Path(p) for p in data.get("default_mapping_files", [])
        ),
        extra_iwyu_args=tuple(data.get("extra_iwyu_args", [])),
        clang_query_path=_opt_path(data.get("clang_query_path")),
        iwyu_path=_opt_path(data.get("iwyu_path")),
        fix_includes_path=_opt_path(data.get("fix_includes_path")),
        allow_missing_clang=policy,
        max_jobs=data.get("max_jobs"),
        cache_root=_opt_path(data.get("cache_root")),
        max_errors=data.get("max_errors"),
    )


def _payload_to_ast_query(d: dict[str, Any]) -> AstQuery:
    body_str = d["matcher_body"]
    body: Any = Path(body_str) if Path(body_str).exists() else body_str
    return AstQuery(
        name=d["name"],
        matcher_body=body,
        scope=_payload_to_scope(d.get("scope")),
        ignore=_payload_to_scope(d.get("ignore")),
        cache_key_namespace=d.get("cache_key_namespace", "").encode("utf-8"),
    )


def _payload_to_iwyu_item(d: dict[str, Any]) -> IwyuItem:
    return IwyuItem(
        name=d["name"],
        mapping_files=tuple(Path(p) for p in d.get("mapping_files", [])),
        pch_in_code=d.get("pch_in_code", False),
        extra_args=tuple(d.get("extra_args", [])),
        auto_fix=d.get("auto_fix", False),
        scope=_payload_to_scope(d.get("scope")),
        ignore=_payload_to_scope(d.get("ignore")),
        cache_key_namespace=d.get("cache_key_namespace", "").encode("utf-8"),
    )


def _payload_to_scope(value: Any) -> Any:
    if value is None:
        return None
    if isinstance(value, list):
        if not value:
            return None
        return tuple(value)
    if isinstance(value, str):
        return value
    raise TypeError(f"unsupported scope payload: {type(value).__name__}")


def _opt_path(value: Any) -> Path | None:
    if value is None:
        return None
    return Path(value)


__all__ = ["dump_lint_input_toml", "load_lint_input_toml"]

"""`cpp_lint(LintInput) -> Iterator[tuple[ResultItem, ...] | Summary]`.

Top-level streaming entry point. Per-path batching semantics:

  - For each TU in the active set, the dispatcher schedules every
    applicable AstQuery (combined into one clang-query process) AND
    every applicable IwyuItem (one IWYU process per item).
  - Per-TU **barrier**: only when all of a TU's sub-jobs complete does
    the runner group that TU's results by source path and emit one
    tuple per path.
  - The streaming guarantee a caller relies on is "the tuple I get for
    path X contains every linter's contribution from this TU for path X."

`order=True` emits TU-barriers in stable per-TU index order via an
in-process min-heap; the heap only releases the next-expected
sequence so callers see byte-identical batch order across runs. Within
one TU the per-path tuple order is stable (sorted by source path).

`max_errors`, `abort_signal`, and exhausted input all terminate the
run cleanly with `Summary.aborted` reporting the cause.

Per-(TU, item) on-disk cache shortcircuits redundant work. Each
successful or deterministic-failure result lands in cache before its
parent TU-barrier completes.
"""

from __future__ import annotations

import os
import threading
import time
from concurrent.futures import Future, ThreadPoolExecutor
from dataclasses import dataclass, field
from heapq import heappop, heappush
from pathlib import Path
from typing import Any, Iterator

from zccache.cpp_lint._cache import (
    DETERMINISTIC_ERROR_KINDS,
    CacheKey,
    LintCache,
    hash_bytes,
    hash_file_contents,
    hash_strings,
)
from zccache.cpp_lint._clang_query import (
    build_combined_script,
    run_clang_query,
)
from zccache.cpp_lint._compile_commands import CompileEntry, parse_compile_commands
from zccache.cpp_lint._iwyu import apply_iwyu_fixes, run_iwyu
from zccache.cpp_lint._listorpath import resolve_to_lines
from zccache.cpp_lint._tools import (
    TOOL_CLANG_QUERY,
    TOOL_FIX_INCLUDES,
    TOOL_IWYU,
    resolve_tools,
)
from zccache.cpp_lint._types import (
    AstQuery,
    CacheStatus,
    IwyuItem,
    LintInput,
    ResultFilter,
    ResultItem,
    ResultKind,
    Summary,
)
from zccache.cpp_lint._validate import validate


def _default_cache_root() -> Path:
    xdg = os.environ.get("XDG_CACHE_HOME")
    base = Path(xdg) if xdg else Path.home() / ".cache"
    return base / "zccache"


# ----- Job + barrier data structures -----


@dataclass
class _SubJob:
    """One AST-combo or one IwyuItem run for a particular TU."""

    family: str                         # "ast" | "iwyu"
    tu_seq: int                         # which _TUWork this belongs to
    tu: Path
    compile_args: tuple[str, ...]
    item_names: tuple[str, ...]
    cache_keys: tuple[CacheKey, ...]
    ast_queries: tuple[AstQuery, ...] = ()
    iwyu_item: IwyuItem | None = None


@dataclass
class _TUWork:
    """All sub-jobs for one TU plus the barrier that gates emission."""

    seq: int
    tu: Path
    pending: int
    collected: list[ResultItem] = field(default_factory=list)
    lock: threading.Lock = field(default_factory=threading.Lock)
    cache_status: CacheStatus = CacheStatus.HIT
    saw_miss: bool = False


@dataclass
class _ReadyBatch:
    """A TU's worth of results, grouped by path, ready to emit as tuples."""

    seq: int
    tu: Path
    cache_status: CacheStatus  # HIT if every sub-job hit cache; else MISS
    grouped: tuple[tuple[ResultItem, ...], ...]  # one tuple per source path


# ----- Public entry point -----


def cpp_lint(
    lint_input: LintInput,
    *,
    cached: bool = True,
    filter_out: ResultFilter = ResultFilter.NONE,
) -> Iterator[tuple[ResultItem, ...] | Summary]:
    """Yield per-path tuples of ResultItem (one tuple per source path
    a completing TU produces), then a final Summary.

    Each yielded tuple groups ALL of one TU's linters' results for one
    source path. The TU barrier guarantees no tuple is emitted until
    every applicable linter for that TU has completed.

    `cached=False` skips cache READS but still writes — useful for
    debugging stale results without polluting on-disk state.

    `filter_out` suppresses items before they leave the runner; errors
    are always kept.
    """
    started = time.monotonic()
    validate(lint_input)
    tool_res = resolve_tools(lint_input)
    cache = LintCache(lint_input.cache_root or _default_cache_root())

    compile_entries = parse_compile_commands(lint_input.compile_commands)
    entries_by_path: dict[str, CompileEntry] = {
        str(e.file): e for e in compile_entries
    }
    active_tus = _active_tus(lint_input, entries_by_path.values())
    sub_jobs, tu_works = _build_jobs(lint_input, active_tus)

    max_jobs = lint_input.max_jobs or max(1, (os.cpu_count() or 1))

    hits = 0
    misses = 0
    successes = 0
    warnings = 0
    errors = 0
    tus_invoked: set[str] = set()
    error_count = 0

    aborted_flag = threading.Event()
    ready_lock = threading.Lock()
    ready_pool: list[_ReadyBatch] = []

    # Optional abort-signal watcher.
    watcher: threading.Thread | None = None
    if lint_input.abort_signal is not None:
        watcher = _spawn_abort_watcher(lint_input.abort_signal, aborted_flag)

    def _on_sub_done(work: _TUWork, items: list[ResultItem], saw_miss: bool) -> None:
        with work.lock:
            work.collected.extend(items)
            work.pending -= 1
            work.saw_miss = work.saw_miss or saw_miss
            completed = work.pending == 0
        if not completed:
            return
        # Group by source path and queue for emission.
        grouped = _group_by_path(work.collected)
        batch = _ReadyBatch(
            seq=work.seq,
            tu=work.tu,
            cache_status=CacheStatus.MISS if work.saw_miss else CacheStatus.HIT,
            grouped=grouped,
        )
        with ready_lock:
            ready_pool.append(batch)

    futures: list[Future[None]] = []
    executor = ThreadPoolExecutor(max_workers=max_jobs)
    try:
        # Dispatch all sub-jobs. Each sub-job notifies its parent _TUWork
        # via _on_sub_done; per-TU emission only fires when pending=0.
        for sj in sub_jobs:
            if aborted_flag.is_set():
                # Compensate the per-TU pending counter for the sub-jobs
                # we never schedule, so the partial barrier still trips.
                work = tu_works[sj.tu_seq]
                with work.lock:
                    work.pending -= 1
                    completed = work.pending == 0
                if completed:
                    grouped = _group_by_path(work.collected)
                    with ready_lock:
                        ready_pool.append(
                            _ReadyBatch(
                                seq=work.seq,
                                tu=work.tu,
                                cache_status=(
                                    CacheStatus.MISS if work.saw_miss else CacheStatus.HIT
                                ),
                                grouped=grouped,
                            )
                        )
                continue
            work = tu_works[sj.tu_seq]
            fut = executor.submit(
                _run_sub_job, sj, work, lint_input, tool_res, cache, cached, _on_sub_done
            )
            futures.append(fut)

        next_emit_seq = 0
        heap: list[tuple[int, _ReadyBatch]] = []
        while True:
            ready_batches = _drain_ready(ready_pool, ready_lock, lint_input.order, next_emit_seq, heap)
            for batch in ready_batches:
                if lint_input.order:
                    next_emit_seq = batch.seq + 1
                if batch.cache_status is CacheStatus.HIT:
                    hits += 1
                else:
                    misses += 1
                tus_invoked.add(str(batch.tu))
                for path_tuple in batch.grouped:
                    counts = _classify_items(path_tuple)
                    successes += counts["successes"]
                    warnings += counts["warnings"]
                    errors += counts["errors"]
                    if lint_input.max_errors is not None and counts["errors"]:
                        error_count += counts["errors"]
                        if error_count >= lint_input.max_errors:
                            aborted_flag.set()
                    filtered = _filter_items(path_tuple, filter_out)
                    if filtered:
                        yield filtered
            done = sum(1 for f in futures if f.done())
            if done >= len(futures) and not ready_pool and not heap:
                break
            time.sleep(0.005)
    finally:
        executor.shutdown(wait=True)
        if watcher is not None:
            watcher.join(timeout=0.2)

    elapsed = time.monotonic() - started
    hit_rate = (hits / (hits + misses)) if (hits + misses) else 0.0
    yield Summary(
        hits=hits,
        misses=misses,
        hit_rate=hit_rate,
        successes=successes,
        warnings=warnings,
        errors=errors,
        tus_invoked=len(tus_invoked),
        elapsed_seconds=elapsed,
        aborted=aborted_flag.is_set(),
        tools_fetched=tool_res.fetched,
        resolved_tool_paths=dict(tool_res.paths),
    )


# ----- Helpers -----


def _spawn_abort_watcher(
    py_event: threading.Event, atomic: threading.Event
) -> threading.Thread:
    def loop() -> None:
        while not atomic.is_set():
            if py_event.wait(0.05):
                atomic.set()
                return

    t = threading.Thread(target=loop, name="cpp_lint-abort", daemon=True)
    t.start()
    return t


def _active_tus(
    lint_input: LintInput, entries: Any
) -> dict[str, CompileEntry]:
    default_scope = resolve_to_lines(lint_input.default_scope)
    default_ignore = resolve_to_lines(lint_input.default_ignore)
    out: dict[str, CompileEntry] = {}
    for entry in entries:
        if _any_item_applies(entry, lint_input, default_scope, default_ignore):
            out[str(entry.file)] = entry
    return out


def _any_item_applies(
    entry: CompileEntry,
    lint_input: LintInput,
    default_scope: tuple[str, ...],
    default_ignore: tuple[str, ...],
) -> bool:
    for q in lint_input.ast_queries:
        scope = resolve_to_lines(q.scope) or default_scope
        ignore = resolve_to_lines(q.ignore) or default_ignore
        if _scope_matches(entry.file, scope, ignore):
            return True
    for r in lint_input.iwyu_items:
        scope = resolve_to_lines(r.scope) or default_scope
        ignore = resolve_to_lines(r.ignore) or default_ignore
        if _scope_matches(entry.file, scope, ignore):
            return True
    return False


def _scope_matches(
    file: Path, scope: tuple[str, ...], ignore: tuple[str, ...]
) -> bool:
    s_file = str(file).replace("\\", "/")
    if not scope:
        included = True
    else:
        included = any(_glob_match(s_file, pat) for pat in scope)
    if not included:
        return False
    if any(_glob_match(s_file, pat) for pat in ignore):
        return False
    return True


def _glob_match(path: str, pattern: str) -> bool:
    import fnmatch

    normalized = pattern.replace("\\", "/")
    if "**" in normalized:
        normalized = normalized.replace("**", "*")
    return fnmatch.fnmatchcase(path.replace("\\", "/"), normalized)


def _build_jobs(
    lint_input: LintInput,
    active_tus: dict[str, CompileEntry],
) -> tuple[list[_SubJob], dict[int, _TUWork]]:
    """Return (flat list of sub-jobs, map[tu_seq → _TUWork barrier])."""
    default_scope = resolve_to_lines(lint_input.default_scope)
    default_ignore = resolve_to_lines(lint_input.default_ignore)

    sub_jobs: list[_SubJob] = []
    tu_works: dict[int, _TUWork] = {}

    for tu_seq, (tu_path, entry) in enumerate(sorted(active_tus.items())):
        applicable_ast = tuple(
            q
            for q in lint_input.ast_queries
            if _scope_matches(
                entry.file,
                resolve_to_lines(q.scope) or default_scope,
                resolve_to_lines(q.ignore) or default_ignore,
            )
        )
        applicable_iwyu = tuple(
            r
            for r in lint_input.iwyu_items
            if _scope_matches(
                entry.file,
                resolve_to_lines(r.scope) or default_scope,
                resolve_to_lines(r.ignore) or default_ignore,
            )
        )
        sub_count = (1 if applicable_ast else 0) + len(applicable_iwyu)
        if sub_count == 0:
            continue
        tu_works[tu_seq] = _TUWork(seq=tu_seq, tu=entry.file, pending=sub_count)

        if applicable_ast:
            sub_jobs.append(
                _SubJob(
                    family="ast",
                    tu_seq=tu_seq,
                    tu=entry.file,
                    compile_args=entry.arguments,
                    item_names=tuple(q.name for q in applicable_ast),
                    cache_keys=tuple(
                        _ast_cache_key(entry, q, default_scope, default_ignore)
                        for q in applicable_ast
                    ),
                    ast_queries=applicable_ast,
                )
            )
        for r in applicable_iwyu:
            scope = resolve_to_lines(r.scope) or default_scope
            ignore = resolve_to_lines(r.ignore) or default_ignore
            sub_jobs.append(
                _SubJob(
                    family="iwyu",
                    tu_seq=tu_seq,
                    tu=entry.file,
                    compile_args=entry.arguments,
                    item_names=(r.name,),
                    cache_keys=(_iwyu_cache_key(entry, r, scope, ignore),),
                    iwyu_item=r,
                )
            )
    return sub_jobs, tu_works


def _tu_fingerprint(entry: CompileEntry) -> bytes:
    file_hash = hash_file_contents(entry.file)
    args_hash = hash_strings(*entry.arguments)
    return hash_bytes(file_hash, args_hash)


def _ast_cache_key(
    entry: CompileEntry,
    q: AstQuery,
    default_scope: tuple[str, ...],
    default_ignore: tuple[str, ...],
) -> CacheKey:
    if isinstance(q.matcher_body, Path) and q.matcher_body.is_file():
        body = q.matcher_body.read_bytes()
    elif isinstance(q.matcher_body, str):
        body = q.matcher_body.encode("utf-8")
    else:
        body = b""
    scope = resolve_to_lines(q.scope) or default_scope
    ignore = resolve_to_lines(q.ignore) or default_ignore
    return LintCache.make_key(
        family="ast",
        tu_fingerprint=_tu_fingerprint(entry),
        item_name=q.name,
        item_config_hash=hash_bytes(body),
        scope_files_hash=hash_strings(*scope, *ignore),
        cache_key_namespace=q.cache_key_namespace,
    )


def _iwyu_cache_key(
    entry: CompileEntry,
    r: IwyuItem,
    scope: tuple[str, ...],
    ignore: tuple[str, ...],
) -> CacheKey:
    mapping_hash = hash_bytes(*[hash_file_contents(mf) for mf in r.mapping_files])
    return LintCache.make_key(
        family="iwyu",
        tu_fingerprint=_tu_fingerprint(entry),
        item_name=r.name,
        item_config_hash=hash_bytes(
            mapping_hash,
            b"\x01" if r.pch_in_code else b"\x00",
            *(arg.encode("utf-8") for arg in r.extra_args),
        ),
        scope_files_hash=hash_strings(*scope, *ignore),
        cache_key_namespace=r.cache_key_namespace,
    )


def _run_sub_job(
    sj: _SubJob,
    work: _TUWork,
    lint_input: LintInput,
    tool_res: Any,
    cache: LintCache,
    cached: bool,
    callback: Any,
) -> None:
    """Execute one sub-job and notify its parent _TUWork barrier."""
    items: list[ResultItem]
    saw_miss = False
    if cached:
        all_hit = True
        cached_items: list[ResultItem] = []
        for item_name, key in zip(sj.item_names, sj.cache_keys):
            payload = cache.get(key)
            if payload is None:
                all_hit = False
                break
            cached_items.extend(_payload_to_items(payload, sj, item_name, CacheStatus.HIT))
        if all_hit:
            callback(work, cached_items, False)
            return
    saw_miss = True
    if sj.family == "ast":
        items = _run_ast_sub(sj, lint_input, tool_res, cache)
    else:
        items = _run_iwyu_sub(sj, lint_input, tool_res, cache)
    callback(work, items, saw_miss)


def _run_ast_sub(
    sj: _SubJob, lint_input: LintInput, tool_res: Any, cache: LintCache
) -> list[ResultItem]:
    script = build_combined_script(
        queries=sj.ast_queries,
        let_bindings=resolve_to_lines(lint_input.let_bindings),
    )
    cq_path = Path(tool_res.paths[TOOL_CLANG_QUERY])
    run = run_clang_query(
        clang_query_path=cq_path,
        tu=sj.tu,
        compile_commands=lint_input.compile_commands,
        script=script,
        extra_args=lint_input.extra_clang_query_args,
    )
    items: list[ResultItem] = []
    if run.error_kind is not None:
        for item_name, key in zip(sj.item_names, sj.cache_keys):
            err = ResultItem(
                path=str(sj.tu),
                kind=ResultKind.AST,
                cache=CacheStatus.MISS,
                message=run.error_message,
                item_name=item_name,
                error=True,
                tu=str(sj.tu),
                extra={"exit_code": str(run.exit_code), "error_kind": run.error_kind},
            )
            items.append(err)
            if run.error_kind in DETERMINISTIC_ERROR_KINDS:
                cache.put(key, _item_to_failure_payload(err))
        return items

    hits_by_name: dict[str, list[Any]] = {name: [] for name in sj.item_names}
    for hit in run.hits:
        bucket = hits_by_name.get(hit.bind_name)
        if bucket is None:
            continue
        bucket.append(hit)
    for item_name, key in zip(sj.item_names, sj.cache_keys):
        bucket = hits_by_name[item_name]
        item_results: list[ResultItem] = []
        for hit in bucket:
            item_results.append(
                ResultItem(
                    path=hit.path,
                    kind=ResultKind.AST,
                    cache=CacheStatus.MISS,
                    message=hit.message,
                    item_name=item_name,
                    warning=True,
                    line=hit.line,
                    column=hit.column,
                    tu=str(sj.tu),
                    extra={},
                )
            )
        items.extend(item_results)
        cache.put(key, _items_to_success_payload(item_results))
    return items


def _run_iwyu_sub(
    sj: _SubJob, lint_input: LintInput, tool_res: Any, cache: LintCache
) -> list[ResultItem]:
    r = sj.iwyu_item
    assert r is not None
    iwyu_path = Path(tool_res.paths[TOOL_IWYU])
    run = run_iwyu(
        iwyu_path=iwyu_path,
        tu=sj.tu,
        item=r,
        compile_args=sj.compile_args,
        default_mapping_files=lint_input.default_mapping_files,
        extra_iwyu_args=lint_input.extra_iwyu_args,
    )
    items: list[ResultItem] = []
    item_name = r.name
    key = sj.cache_keys[0]

    if run.error_kind is not None:
        err = ResultItem(
            path=str(sj.tu),
            kind=ResultKind.IWYU,
            cache=CacheStatus.MISS,
            message=run.error_message,
            item_name=item_name,
            error=True,
            tu=str(sj.tu),
            extra={"exit_code": str(run.exit_code), "error_kind": run.error_kind},
        )
        items.append(err)
        if run.error_kind in DETERMINISTIC_ERROR_KINDS:
            cache.put(key, _item_to_failure_payload(err))
        return items

    fixed_files: tuple[str, ...] = ()
    if r.auto_fix and TOOL_FIX_INCLUDES in tool_res.paths:
        fix_path = Path(tool_res.paths[TOOL_FIX_INCLUDES])
        fixed_files = apply_iwyu_fixes(fix_path, run)

    for r_item in run.items:
        warning = r_item.action != "keep"
        extra: dict[str, str] = {
            "action": r_item.action,
            "include": r_item.spelling,
        }
        if r.auto_fix and r_item.path in fixed_files and r_item.action in ("add", "remove"):
            extra["fix_applied"] = "true"
        items.append(
            ResultItem(
                path=r_item.path,
                kind=ResultKind.IWYU,
                cache=CacheStatus.MISS,
                message=r_item.reason,
                item_name=item_name,
                warning=warning,
                tu=str(sj.tu),
                extra=extra,
            )
        )
    cache.put(key, _items_to_success_payload(items))
    return items


def _items_to_success_payload(items: list[ResultItem]) -> dict[str, Any]:
    return {
        "kind": "success",
        "items": [
            {
                "path": it.path,
                "kind_family": it.kind.value,
                "message": it.message,
                "item_name": it.item_name,
                "error": it.error,
                "warning": it.warning,
                "line": it.line,
                "column": it.column,
                "extra": dict(it.extra),
            }
            for it in items
        ],
    }


def _item_to_failure_payload(it: ResultItem) -> dict[str, Any]:
    return {
        "kind": "failure",
        "path": it.path,
        "kind_family": it.kind.value,
        "item_name": it.item_name,
        "message": it.message,
        "exit_code": int(it.extra.get("exit_code", "-1")),
        "error_kind": it.extra.get("error_kind", "INTERNAL"),
        "extra": dict(it.extra),
    }


def _payload_to_items(
    payload: dict[str, Any], sj: _SubJob, item_name: str, cache_status: CacheStatus
) -> list[ResultItem]:
    if payload.get("kind") == "failure":
        return [
            ResultItem(
                path=payload["path"],
                kind=ResultKind(payload["kind_family"]),
                cache=cache_status,
                message=payload["message"],
                item_name=payload["item_name"],
                error=True,
                tu=str(sj.tu),
                extra=dict(payload.get("extra", {})),
            )
        ]
    out: list[ResultItem] = []
    for entry in payload.get("items", []):
        out.append(
            ResultItem(
                path=entry["path"],
                kind=ResultKind(entry["kind_family"]),
                cache=cache_status,
                message=entry["message"],
                item_name=entry["item_name"],
                error=bool(entry.get("error", False)),
                warning=bool(entry.get("warning", False)),
                line=int(entry.get("line", 0)),
                column=int(entry.get("column", 0)),
                tu=str(sj.tu),
                extra=dict(entry.get("extra", {})),
            )
        )
    return out


def _group_by_path(items: list[ResultItem]) -> tuple[tuple[ResultItem, ...], ...]:
    by_path: dict[str, list[ResultItem]] = {}
    for it in items:
        by_path.setdefault(it.path, []).append(it)
    return tuple(tuple(by_path[p]) for p in sorted(by_path))


def _filter_items(
    items: tuple[ResultItem, ...], policy: ResultFilter
) -> tuple[ResultItem, ...]:
    if policy is ResultFilter.NONE:
        return items
    if policy is ResultFilter.ALL_BUT_ERRORS:
        return tuple(it for it in items if it.error)
    if policy is ResultFilter.SUCCESSES:
        return tuple(it for it in items if it.error or it.warning)
    return items  # pragma: no cover


def _classify_items(items: tuple[ResultItem, ...]) -> dict[str, int]:
    successes = warnings = errors = 0
    for it in items:
        if it.error:
            errors += 1
        elif it.warning:
            warnings += 1
        else:
            successes += 1
    return {"successes": successes, "warnings": warnings, "errors": errors}


def _drain_ready(
    pool: list[_ReadyBatch],
    lock: threading.Lock,
    ordered: bool,
    next_seq: int,
    heap: list[tuple[int, _ReadyBatch]],
) -> list[_ReadyBatch]:
    emit: list[_ReadyBatch] = []
    with lock:
        if not ordered:
            emit.extend(pool)
            pool.clear()
            return emit
        for batch in pool:
            heappush(heap, (batch.seq, batch))
        pool.clear()
        while heap and heap[0][0] == next_seq:
            emit.append(heappop(heap)[1])
            next_seq += 1
    return emit


__all__ = ["cpp_lint"]

"""Cross-platform Python watcher API backed by a Rust polling engine."""

from __future__ import annotations

import _thread
import os
import queue
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

from zccache.watcher._native import NativeWatcher


def file_watcher_enabled() -> bool:
    return os.getenv("NO_FILE_WATCHING", "0") != "1"


def file_watcher_set(enabled: bool) -> None:
    os.environ["NO_FILE_WATCHING"] = "0" if enabled else "1"


def _log_keyboard_interrupt_stop() -> None:
    print("KeyboardInterrupt: watcher stopped")


def handle_keyboard_interrupt(ke: KeyboardInterrupt) -> None:
    if threading.current_thread() is threading.main_thread():
        raise KeyboardInterrupt() from ke
    _thread.interrupt_main()


NotificationPredicate = Callable[..., bool]


@dataclass(frozen=True)
class FileChangeEvent:
    changed: tuple[str, ...]
    removed: tuple[str, ...]
    overflow: bool = False

    @property
    def paths(self) -> list[str]:
        return sorted(set(self.changed) | set(self.removed))


@dataclass(frozen=True)
class NormalizedPaths:
    paths: list[str]


def _normalize_path_str(path: str | os.PathLike[str]) -> str:
    raw = str(path)
    if raw.startswith("\\\\?\\"):
        raw = raw[4:]
    return str(Path(raw).resolve())


def _normalize_paths(paths: list[str]) -> NormalizedPaths:
    return NormalizedPaths(
        paths=sorted(dict.fromkeys(_normalize_path_str(path) for path in paths))
    )


def _relative_to_root(path: Path, root: Path) -> str:
    normalized = Path(_normalize_path_str(path))
    try:
        return normalized.relative_to(root).as_posix()
    except ValueError:
        return normalized.as_posix()


class FileWatcher:
    """User-facing watcher with Rust-side polling and Python-side delivery hooks."""

    def __init__(
        self,
        root: str | os.PathLike[str],
        *,
        include_folders: list[str | os.PathLike[str]] | None = None,
        include_globs: list[str] | None = None,
        exclude_globs: list[str] | None = None,
        excluded_patterns: list[str] | None = None,
        debounce_seconds: float = 0.2,
        poll_interval: float = 0.1,
        callback: Callable[[FileChangeEvent], None] | None = None,
        notification_predicate: NotificationPredicate | None = None,
        autostart: bool = True,
    ) -> None:
        self.root = Path(root).resolve()
        self.notification_predicate = notification_predicate
        self.poll_interval = poll_interval
        self._callbacks: list[Callable[[FileChangeEvent], None]] = []
        if callback is not None:
            self._callbacks.append(callback)
        self._queue: queue.Queue[FileChangeEvent] = queue.Queue()
        self._dispatch_stop = threading.Event()
        self._dispatch_thread: threading.Thread | None = None
        self._keyboard_interrupt_logged = False
        patterns = list(excluded_patterns or [])
        if exclude_globs:
            patterns.extend(exclude_globs)
        self._native = NativeWatcher(
            str(self.root),
            include_folders=[str(Path(folder)) for folder in (include_folders or [])],
            include_globs=list(include_globs or []),
            excluded_patterns=patterns,
            poll_interval_ms=max(1, int(poll_interval * 1000)),
            debounce_ms=max(0, int(debounce_seconds * 1000)),
        )
        self._started = False
        if autostart:
            self.start()

    @property
    def is_running(self) -> bool:
        return self._native.is_running()

    def start(self) -> None:
        if not self.is_running:
            self._clear_queue()
            self._native.start()
            self._start_dispatch_thread()
        self._started = True

    def resume(self) -> None:
        self.start()

    def stop(self) -> None:
        self._dispatch_stop.set()
        self._native.stop()
        if self._dispatch_thread is not None:
            self._dispatch_thread.join(timeout=2.0)
            self._dispatch_thread = None
        self._clear_queue()

    def close(self) -> None:
        self.stop()

    def __enter__(self) -> "FileWatcher":
        if not self._started or not self.is_running:
            self.start()
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.stop()

    def add_callback(self, callback: Callable[[FileChangeEvent], None]) -> None:
        self._callbacks.append(callback)

    def poll(self, timeout: float | None = None) -> FileChangeEvent | None:
        if not file_watcher_enabled():
            return None
        try:
            if timeout is None:
                return self._queue.get_nowait()
            return self._queue.get(timeout=timeout)
        except KeyboardInterrupt:
            raise
        except queue.Empty:
            return None

    def _event_from_batch(
        self,
        changed: list[str],
        removed: list[str],
        overflow: bool,
    ) -> FileChangeEvent | None:
        normalized_changed = _normalize_paths(changed)
        normalized_removed = _normalize_paths(removed)
        keep_changed = [
            path for path in normalized_changed.paths if self._predicate_allows(path, "changed")
        ]
        keep_removed = [
            path for path in normalized_removed.paths if self._predicate_allows(path, "removed")
        ]
        if not keep_changed and not keep_removed and not overflow:
            return None
        return FileChangeEvent(
            changed=tuple(keep_changed),
            removed=tuple(keep_removed),
            overflow=overflow,
        )

    def _predicate_allows(self, path_str: str, change: str) -> bool:
        if self.notification_predicate is None:
            return True
        path = Path(_normalize_path_str(path_str))
        try:
            return bool(
                self.notification_predicate(
                    path,
                    relative_path=_relative_to_root(path, self.root),
                    change=change,
                    root=self.root,
                )
            )
        except KeyboardInterrupt:
            # The dispatch loop owns main-thread notification. Re-raise so it
            # can request exactly one interruption for this watcher.
            self._log_keyboard_interrupt_stop_once()
            raise
        except Exception:
            return True

    def _start_dispatch_thread(self) -> None:
        self._dispatch_stop = threading.Event()
        self._dispatch_thread = threading.Thread(
            target=self._dispatch_loop,
            name="zccache-watcher-dispatch",
            daemon=True,
        )
        self._dispatch_thread.start()

    def _dispatch_loop(self) -> None:
        timeout_ms = max(1, int(self.poll_interval * 1000))
        try:
            while not self._dispatch_stop.is_set():
                batch = self._native.poll_batch(timeout_ms)
                if batch is None:
                    continue
                event = self._event_from_batch(batch.changed, batch.removed, batch.overflow)
                if event is None:
                    continue
                self._queue.put(event)
                for callback in list(self._callbacks):
                    callback(event)
        except KeyboardInterrupt as ke:
            self._log_keyboard_interrupt_stop_once()
            handle_keyboard_interrupt(ke)
            return

    def _clear_queue(self) -> None:
        while True:
            try:
                self._queue.get_nowait()
            except queue.Empty:
                return

    def _log_keyboard_interrupt_stop_once(self) -> None:
        if self._keyboard_interrupt_logged:
            return
        self._keyboard_interrupt_logged = True
        _log_keyboard_interrupt_stop()


def watch_files(
    root: str | os.PathLike[str],
    *,
    include_folders: list[str | os.PathLike[str]] | None = None,
    include_globs: list[str] | None = None,
    exclude_globs: list[str] | None = None,
    excluded_patterns: list[str] | None = None,
    debounce_seconds: float = 0.2,
    poll_interval: float = 0.1,
    callback: Callable[[FileChangeEvent], None] | None = None,
    notification_predicate: NotificationPredicate | None = None,
    autostart: bool = True,
) -> FileWatcher:
    return FileWatcher(
        root,
        include_folders=include_folders,
        include_globs=include_globs,
        exclude_globs=exclude_globs,
        excluded_patterns=excluded_patterns,
        debounce_seconds=debounce_seconds,
        poll_interval=poll_interval,
        callback=callback,
        notification_predicate=notification_predicate,
        autostart=autostart,
    )


class FileWatcherProcess:
    def __init__(
        self,
        root: Path,
        excluded_patterns: list[str],
        *,
        include_folders: list[str | os.PathLike[str]] | None = None,
        include_globs: list[str] | None = None,
        debounce_seconds: float = 0.2,
        poll_interval: float = 0.1,
        callback: Callable[[FileChangeEvent], None] | None = None,
        notification_predicate: NotificationPredicate | None = None,
    ) -> None:
        self.root = Path(root).resolve()
        self._watcher = FileWatcher(
            self.root,
            include_folders=include_folders,
            include_globs=include_globs,
            excluded_patterns=excluded_patterns,
            debounce_seconds=debounce_seconds,
            poll_interval=poll_interval,
            callback=callback,
            notification_predicate=notification_predicate,
        )

    def stop(self) -> None:
        self._watcher.stop()

    def add_callback(self, callback: Callable[[FileChangeEvent], None]) -> None:
        self._watcher.add_callback(callback)

    def poll(self, timeout: float | None = None) -> FileChangeEvent | None:
        return self._watcher.poll(timeout)

    def get_all_changes(self, timeout: float | None = None) -> list[str]:
        event = self._watcher.poll(timeout)
        if event is None:
            return []
        changed = set(event.paths)
        while True:
            event = self._watcher.poll(0)
            if event is None:
                break
            changed.update(event.paths)
        return sorted(changed)


class DebouncedFileWatcherProcess:
    def __init__(self, watcher: FileWatcherProcess, debounce_seconds: float = 0.2) -> None:
        self.watcher = watcher
        self.debounce_seconds = debounce_seconds
        self.last_event_time: float | None = None

    def get_all_changes(self, timeout: float | None = None) -> list[str]:
        changes = self.watcher.get_all_changes(timeout)
        if not changes:
            return []
        self.last_event_time = time.time()
        return changes

    def stop(self) -> None:
        self.watcher.stop()


__all__ = [
    "DebouncedFileWatcherProcess",
    "FileChangeEvent",
    "FileWatcher",
    "FileWatcherProcess",
    "NotificationPredicate",
    "file_watcher_enabled",
    "file_watcher_set",
    "handle_keyboard_interrupt",
    "watch_files",
]

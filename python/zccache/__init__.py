"""Python bindings for the zccache watcher, fingerprint, and daemon APIs."""

from __future__ import annotations

from zccache import client, cpp_lint, downloader, fingerprint, ino, watcher
from zccache.client import SessionStartResult, SessionStats, ZcCacheClient
from zccache.cpp_lint import (
    AstQuery,
    CacheStatus,
    IwyuItem,
    LintInput,
    LintInputError,
    MissingClangPolicy,
    ResultFilter,
    ResultItem,
    ResultKind,
    Summary,
    cpp_lint as cpp_lint_run,
)
from zccache.downloader import (
    DownloadApi,
    DownloadDaemonStatus,
    DownloadHandle,
    DownloadStatus,
    FetchResult,
    FetchState,
)
from zccache.fingerprint import (
    Api,
    FingerprintCache,
    FingerprintDecision,
    FingerprintManager,
    FingerprintResult,
)
from zccache.ino import InoConvertResult, convert_ino
from zccache.watcher import (
    DebouncedFileWatcherProcess,
    FileChangeEvent,
    FileWatcher,
    FileWatcherProcess,
    watch_files,
)

__all__ = [
    "Api",
    "AstQuery",
    "CacheStatus",
    "DebouncedFileWatcherProcess",
    "DownloadApi",
    "DownloadDaemonStatus",
    "DownloadHandle",
    "DownloadStatus",
    "FetchResult",
    "FetchState",
    "FileChangeEvent",
    "FileWatcher",
    "FileWatcherProcess",
    "FingerprintCache",
    "FingerprintDecision",
    "FingerprintManager",
    "FingerprintResult",
    "InoConvertResult",
    "IwyuItem",
    "LintInput",
    "LintInputError",
    "MissingClangPolicy",
    "ResultFilter",
    "ResultItem",
    "ResultKind",
    "SessionStartResult",
    "SessionStats",
    "Summary",
    "ZcCacheClient",
    "client",
    "convert_ino",
    "cpp_lint",
    "cpp_lint_run",
    "downloader",
    "fingerprint",
    "ino",
    "watcher",
    "watch_files",
]

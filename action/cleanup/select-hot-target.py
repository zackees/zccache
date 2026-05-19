#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path


MUST_KEEP_NAMES = {
    "CACHEDIR.TAG",
    ".rustc_info.json",
    # Sidecar manifest written by `zccache snapshot-fp-record`. Must ride
    # the tar so `snapshot-fp-validate` on the restore side can read it.
    ".zccache-fp-manifest.json",
}
MUST_KEEP_SUFFIXES = {".d", ".rmeta"}
BUILD_METADATA_NAMES = {"output", "invoked.timestamp", "root-output"}


def _is_incremental(path: Path) -> bool:
    return "incremental" in path.parts


def _is_build_out(path: Path) -> bool:
    return "build" in path.parts and "out" in path.parts


def _is_cargo_metadata(path: Path) -> bool:
    if path.name in MUST_KEEP_NAMES:
        return True
    if path.suffix in MUST_KEEP_SUFFIXES:
        return True
    if ".fingerprint" in path.parts:
        return True
    return "build" in path.parts and path.name in BUILD_METADATA_NAMES


def select_hot_files(
    target: Path,
    marker_epoch: float,
    prune_build_script_out: bool,
) -> tuple[list[str], dict[str, int]]:
    selected: list[str] = []
    seen_inodes: set[tuple[int, int]] = set()
    stats = {
        "visited_files": 0,
        "selected_files": 0,
        "selected_bytes": 0,
        "skipped_files": 0,
        "skipped_bytes": 0,
        "metadata_files": 0,
        "hot_files": 0,
    }

    for file_path in target.rglob("*"):
        if not file_path.is_file():
            continue
        rel = file_path.relative_to(target)
        stats["visited_files"] += 1
        try:
            stat = file_path.stat()
        except OSError:
            stats["skipped_files"] += 1
            continue

        inode_key = (stat.st_dev, stat.st_ino)
        size = 0 if inode_key in seen_inodes else stat.st_size
        seen_inodes.add(inode_key)

        if _is_incremental(rel) or (prune_build_script_out and _is_build_out(rel)):
            stats["skipped_files"] += 1
            stats["skipped_bytes"] += size
            continue

        metadata = _is_cargo_metadata(rel)
        hot = marker_epoch > 0 and (
            stat.st_mtime >= marker_epoch or stat.st_atime >= marker_epoch
        )
        if not metadata and not hot:
            stats["skipped_files"] += 1
            stats["skipped_bytes"] += size
            continue

        selected.append(rel.as_posix())
        stats["selected_files"] += 1
        stats["selected_bytes"] += size
        if metadata:
            stats["metadata_files"] += 1
        if hot:
            stats["hot_files"] += 1

    return selected, stats


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", required=True)
    parser.add_argument("--marker-epoch", type=float, default=0)
    parser.add_argument("--prune-build-script-out", action="store_true")
    parser.add_argument("--list-file", required=True)
    args = parser.parse_args()

    selected, stats = select_hot_files(
        target=Path(args.target),
        marker_epoch=args.marker_epoch,
        prune_build_script_out=args.prune_build_script_out,
    )
    list_file = Path(args.list_file)
    list_file.parent.mkdir(parents=True, exist_ok=True)
    with open(list_file, "wb") as fh:
        for item in selected:
            fh.write(os.fsencode(item))
            fh.write(b"\0")
    print(json.dumps(stats, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

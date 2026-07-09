#!/usr/bin/env python3
"""Build a `manifest.json` release asset enumerating the published binaries.

Issue #858: soldr's binary-fetch path currently lists a repo's releases via the
unauthenticated GitHub API (`GET /repos/.../releases`), which is rate-limited
per IP (shared across every CI job on a runner-pool IP) and 403s on busy macOS
pools — forcing a 4-6 min `cargo install` fallback. Publishing a static
`manifest.json` alongside each release lets soldr resolve the exact binary via
the release-asset CDN (no API call, no rate limit).

The manifest is generated in the `publish-release` job after the archives are
downloaded into one directory, so it enumerates whatever archives are actually
present (robust to matrix changes). It is written into that same directory and
swept up by the workflow's `files: release-assets/*` upload glob.

Schema (the contract soldr reads):

    {
      "version": "1.12.15",
      "tag": "v1.12.15",
      "binaries": [
        {
          "target": "x86_64-unknown-linux-musl",
          "asset": "zccache-v1.12.15-x86_64-unknown-linux-musl.tar.gz",
          "url": "https://github.com/<repo>/releases/download/<tag>/<asset>",
          "sha256": "<hex>"
        },
        ...
      ]
    }

Only the primary per-target binary archives are listed; `-debug` symbol
archives are intentionally excluded (soldr wants the runnable binary, and a
`-debug` entry would collide on the `target` key).
"""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path

# Primary binary archive extensions, longest first so `.tar.gz` is matched
# before a hypothetical `.gz`.
ARCHIVE_EXTS = (".tar.gz", ".zip")
DEBUG_MARKER = "-debug"


def normalize_version(version: str) -> str:
    """Force the `v` prefix used in asset filenames (mirrors package_release)."""
    return version if version.startswith("v") else f"v{version}"


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def parse_target(filename: str, version_tag: str) -> str | None:
    """Extract the target triple from a primary binary archive filename.

    Returns None for anything that is not a primary `zccache-<vtag>-<target>`
    archive (installers, checksums, `-debug` archives, unrelated files).
    """
    prefix = f"zccache-{version_tag}-"
    if not filename.startswith(prefix):
        return None
    for ext in ARCHIVE_EXTS:
        if filename.endswith(ext):
            middle = filename[len(prefix) : -len(ext)]
            if not middle or DEBUG_MARKER in middle:
                return None
            return middle
    return None


def build_manifest(assets_dir: Path, version: str, tag: str, repo: str) -> dict:
    version_tag = normalize_version(version)
    binaries = []
    for path in sorted(assets_dir.iterdir()):
        if not path.is_file():
            continue
        target = parse_target(path.name, version_tag)
        if target is None:
            continue
        binaries.append(
            {
                "target": target,
                "asset": path.name,
                "url": f"https://github.com/{repo}/releases/download/{tag}/{path.name}",
                "sha256": sha256_file(path),
            }
        )
    # Deterministic order regardless of filesystem iteration.
    binaries.sort(key=lambda entry: entry["target"])
    return {
        "version": version.lstrip("v"),
        "tag": tag,
        "binaries": binaries,
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--assets-dir", type=Path, required=True, help="Directory of downloaded release archives")
    parser.add_argument("--version", required=True, help="Release version, e.g. 1.12.15 or v1.12.15")
    parser.add_argument("--tag", required=True, help="Actual release tag used in the download URL (may be bare or v-prefixed)")
    parser.add_argument("--repo", default="zackees/zccache", help="owner/repo for the download URL")
    parser.add_argument("--output", type=Path, default=None, help="Manifest output path (default: <assets-dir>/manifest.json)")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    manifest = build_manifest(args.assets_dir, args.version, args.tag, args.repo)
    output = args.output or (args.assets_dir / "manifest.json")
    output.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print(f"wrote {output} with {len(manifest['binaries'])} binaries")
    for entry in manifest["binaries"]:
        print(f"  {entry['target']} -> {entry['asset']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env bash
# Extract a fixture into a working directory.
#
# Usage: extract.sh <fixture-name> <dest-dir>
# Result: the fixture's source tree lives at <dest-dir>/<fixture-name>/
set -euo pipefail

if (( $# != 2 )); then
    echo "usage: extract.sh <fixture-name> <dest-dir>" >&2
    exit 2
fi

FIXTURE="$1"
DEST="$2"

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${HERE}/../.." && pwd)"
TARBALL="${REPO_ROOT}/perf/fixtures/${FIXTURE}.tar.gz"

if [[ ! -f "${TARBALL}" ]]; then
    echo "extract.sh: tarball not found: ${TARBALL}" >&2
    exit 2
fi

mkdir -p "${DEST}"
tar -C "${DEST}" -xzf "${TARBALL}"
echo "extract.sh: ${TARBALL} -> ${DEST}/${FIXTURE}"

#!/usr/bin/env bash
# Regenerate perf/fixtures/medium.tar.gz from perf/fixtures/medium/.
#
# Run this after editing the fixture source (Cargo.toml, src/*.rs) and
# commit both the source-tree changes and the resulting .tar.gz. The
# tarball is checked in so workers do not need a Rust toolchain to
# materialise the fixture before benchmarking — they just untar.
#
# Re-resolving dependencies (cargo generate-lockfile) is intentionally
# left as a separate manual step: that way regenerating the tarball
# never bumps transitive versions silently.
set -euo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "${HERE}"

FIXTURE="${1:-medium}"
if [[ ! -d "${FIXTURE}" ]]; then
    echo "regen.sh: no such fixture directory: ${FIXTURE}" >&2
    echo "  available fixtures:" >&2
    for d in */; do
        [[ -f "${d%/}/Cargo.toml" ]] && echo "    - ${d%/}" >&2
    done
    exit 2
fi

if [[ ! -f "${FIXTURE}/Cargo.lock" ]]; then
    echo "regen.sh: ${FIXTURE}/Cargo.lock is missing — run" >&2
    echo "  (cd perf/fixtures/${FIXTURE} && soldr cargo generate-lockfile)" >&2
    echo "  before regenerating the tarball." >&2
    exit 2
fi

OUTPUT="${FIXTURE}.tar.gz"

# Tar directly out of the fixture directory with explicit excludes so
# target/, stale tarballs, and other build droppings never leak into
# the archive. Pinning --mtime='@0' + --sort=name keeps the tarball
# byte-deterministic across machines so a regen with no source change
# produces no diff.
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime='@0' \
    --exclude="${FIXTURE}/target" \
    --exclude="${FIXTURE}/.DS_Store" \
    --exclude='*.tar.gz' \
    -C "${HERE}" -czf "${OUTPUT}" "${FIXTURE}"

bytes="$(wc -c <"${OUTPUT}")"
echo "regen.sh: wrote ${OUTPUT} (${bytes} bytes)"

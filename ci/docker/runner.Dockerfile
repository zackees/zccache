# syntax=docker/dockerfile:1.7
#
# Third image of the local perf harness — actually runs a perf
# scenario against pre-built binaries.
#
# This image has NO compiler installed (well, rustc is present because
# the base is rust:1.94.1, but the runner doesn't build anything itself).
# Instead it mounts in:
#   - the soldr binary produced by soldr-builder
#   - the zccache source (read-only, for perf/scenarios/, perf/lib/,
#     perf/fixtures/)
#   - a results dir on the host
#
# It then runs the perf scenario script and copies result.json + cache
# reports + logs back to the mounted results dir.
#
# Why share the rust:1.94.1-bookworm-slim base with the zccache builder:
# matches glibc + libssl ABI exactly, so anything the zccache binaries
# linked against at build time resolves at run time.
#
# Run via ci/perf_local.py:
#
#   docker run --rm \
#     -v <repo>/.perf-local/binaries/soldr/soldr:/usr/local/bin/soldr:ro \
#     -v <repo>:/zccache-src:ro \
#     -v <repo>/.perf-local/results/<scenario>:/results \
#     -e SCENARIO=cold-tar-untar-warm \
#     -e FIXTURE=medium \
#     zccache-perf-runner

FROM rust:1.94.1-slim-bookworm

# Scenario script deps. tar+zstd let `soldr save`/`soldr load` round-trip
# the .tar.zst snapshots; jq parses result.json + cache reports.
# `time` is GNU time, used in some scenario bookkeeping. ca-certificates
# is belt-and-braces for any HTTPS soldr might do at runtime.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        bash \
        tar \
        zstd \
        jq \
        time \
        ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# The entrypoint expects all the above mounts to be in place. It then:
#   1. bash perf/lib/extract.sh $FIXTURE $WORK_DIR
#   2. bash perf/scenarios/$SCENARIO/run.sh $WORK_DIR/$FIXTURE
#   3. Copy result.json + *-cache-report.json + *-zccache-logs/ to /results
COPY perf_entrypoint.sh /usr/local/bin/perf_entrypoint.sh
RUN chmod +x /usr/local/bin/perf_entrypoint.sh

ENTRYPOINT ["/usr/local/bin/perf_entrypoint.sh"]

# syntax=docker/dockerfile:1.7
#
# Persistent build environment for soldr-cli (x86_64-unknown-linux-gnu).
#
# This image is NOT a one-shot builder — it carries the rust toolchain + GNU
# build tools + git, but the actual source mount and target/ cache come from
# host-side volumes at run time. That makes source-only changes a cargo
# recompile (seconds) instead of a Docker layer-cache miss (minutes).
#
# The orchestrator (ci/perf_local.py) runs this image like:
#
#   docker run --rm \
#     -v <repo>/.perf-local/soldr-src:/src:ro \
#     -v <repo>/.perf-local/target/soldr:/target \
#     -v <repo>/.perf-local/binaries/soldr:/out \
#     zccache-perf-soldr-builder
#
# Match the runner ABI: soldr selects managed tools from its compiled host
# target. A musl binary runs on glibc but selects unsupported musl LLVM tools.

FROM rust:1.95.0-slim-bookworm

RUN apt-get update && apt-get install -y --no-install-recommends \
    git ca-certificates \
    && apt-get clean

# The orchestrator mounts a persistent /target so cargo incremental wins
# across runs. CARGO_TARGET_DIR redirects all build output there without
# requiring `cargo build --target-dir=...` plumbing.
ENV CARGO_TARGET_DIR=/target

# Same trick for cargo's registry / git checkouts — keep them in a volume
# so a fresh container reuses last run's downloaded crates.
ENV CARGO_HOME=/cargo-home

WORKDIR /src

# Entrypoint: build soldr-cli for GNU, then publish the binary
# to /out/soldr where the runner image can volume-mount it.
#
# Exit non-zero if /src is not bind-mounted (the image is useless without
# a source mount, so the failure mode should be loud).
COPY <<'EOF' /usr/local/bin/build.sh
#!/bin/sh
set -eu
if [ ! -f /src/Cargo.toml ]; then
    echo "ERROR: /src is not bind-mounted (no Cargo.toml found)" >&2
    echo "       Mount soldr source as: -v <soldr-checkout>:/src:ro" >&2
    exit 2
fi
mkdir -p /out
cargo build --release --target x86_64-unknown-linux-gnu -p soldr-cli
cp "${CARGO_TARGET_DIR}/x86_64-unknown-linux-gnu/release/soldr" /out/soldr
echo "wrote /out/soldr  ($(stat -c %s /out/soldr) bytes)"
EOF
RUN chmod +x /usr/local/bin/build.sh

ENTRYPOINT ["/usr/local/bin/build.sh"]

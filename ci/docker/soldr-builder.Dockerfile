# syntax=docker/dockerfile:1.7
#
# Persistent build environment for soldr-cli (static, x86_64-unknown-linux-musl).
#
# This image is NOT a one-shot builder — it carries the rust toolchain + musl
# headers + git, but the actual source mount and target/ cache come from
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
# Why musl: the resulting binary is static, so it runs on the (glibc) runner
# image without any libc compatibility worry.

FROM rust:1.95.0-alpine

# musl-dev: musl libc headers (the `+crt-static` target needs them).
# git: cargo's git-dep resolution + Cargo.lock fetch.
# ca-certificates: HTTPS to crates.io.
RUN apk add --no-cache musl-dev git ca-certificates

# Add the musl target once at image-build time so per-run cargo invocations
# don't pay the download cost.
RUN rustup target add x86_64-unknown-linux-musl

# The orchestrator mounts a persistent /target so cargo incremental wins
# across runs. CARGO_TARGET_DIR redirects all build output there without
# requiring `cargo build --target-dir=...` plumbing.
ENV CARGO_TARGET_DIR=/target

# Same trick for cargo's registry / git checkouts — keep them in a volume
# so a fresh container reuses last run's downloaded crates.
ENV CARGO_HOME=/cargo-home

WORKDIR /src

# Entrypoint: build soldr-cli for musl, then publish the static binary
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
cargo build --release --target x86_64-unknown-linux-musl -p soldr-cli
cp "${CARGO_TARGET_DIR}/x86_64-unknown-linux-musl/release/soldr" /out/soldr
echo "wrote /out/soldr  ($(stat -c %s /out/soldr) bytes)"
EOF
RUN chmod +x /usr/local/bin/build.sh

ENTRYPOINT ["/usr/local/bin/build.sh"]

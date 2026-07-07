# syntax=docker/dockerfile:1.7
#
# Persistent build environment for the zccache trio
# (zccache, zccache-daemon, zccache-fp) targeting glibc x86_64.
#
# Same shape as soldr-builder.Dockerfile (volume-mounted source +
# persistent target/) but on bookworm-slim because:
#   1. We want a glibc binary so the runner image (also bookworm) has
#      no dynamic-link surprises when soldr invokes zccache-daemon.
#   2. bookworm is the same OS family as the catthehacker/ubuntu act-24.04
#      image, so binary behaviour matches the GHA perf-cluster runner
#      as closely as we can without literally booting the same image.
#
# Run via ci/perf_local.py:
#
#   docker run --rm \
#     -v <repo>:/src:ro \
#     -v <repo>/.perf-local/target/zccache:/target \
#     -v <repo>/.perf-local/binaries/zccache:/out \
#     zccache-perf-zccache-builder

FROM rust:1.94.1-slim-bookworm

# Build deps for any cc-rs / pkg-config / C-FFI crates that show up in
# zccache's transitive graph. Notable consumers:
#   - libssl-dev: openssl-sys
#   - pkg-config: every -sys crate that uses it for system-lib discovery
#   - build-essential: cc-rs link step for mimalloc and other -sys crates
# (autoconf was here for tikv-jemalloc-sys' autogen.sh; dropped now that
# mimalloc — pure C, no autotools — is the sole allocator.)
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        build-essential \
        pkg-config \
        libssl-dev \
        ca-certificates \
        curl \
        git \
 && rm -rf /var/lib/apt/lists/*

ENV CARGO_TARGET_DIR=/target
ENV CARGO_HOME=/cargo-home

WORKDIR /src

# Build the three binaries the perf-cluster's bench job stages.
# `--bin zccache --bin zccache-daemon --bin zccache-fp` matches the
# `cargo build` invocation in .github/workflows/perf-rust-cluster.yml
# (step "Build zccache trio (release)") so what we measure locally is
# byte-for-byte equivalent to what the cluster would have measured.
COPY <<'EOF' /usr/local/bin/build.sh
#!/bin/sh
set -eu
if [ ! -f /src/Cargo.toml ]; then
    echo "ERROR: /src is not bind-mounted (no Cargo.toml found)" >&2
    echo "       Mount the zccache repo as: -v <zccache-repo>:/src:ro" >&2
    exit 2
fi
mkdir -p /out
cargo build --release \
    --features zccache-bin,daemon-bin,fingerprint-bin \
    --bin zccache \
    --bin zccache-daemon \
    --bin zccache-fp
for bin in zccache zccache-daemon zccache-fp; do
    cp "${CARGO_TARGET_DIR}/release/${bin}" "/out/${bin}"
    chmod +x "/out/${bin}"
done
echo "wrote /out/{zccache,zccache-daemon,zccache-fp}:"
ls -l /out/
EOF
RUN chmod +x /usr/local/bin/build.sh

ENTRYPOINT ["/usr/local/bin/build.sh"]

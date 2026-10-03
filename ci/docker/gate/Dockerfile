# syntax=docker/dockerfile:1.7

# Isolated Linux environment for the local gate's `tests` lane
# (zackees/ci.yml#168 GATE-005, #196 GATE-009). zccache's test suite starts
# zccache daemons and touches zccache state roots, so it never runs on a
# developer host; `bosn run --task gate-test` runs it here instead. The
# package set mirrors what the hosted ubuntu-24.04 Integration runner gives
# the suite (clang, a C/C++ toolchain, CMake/Ninja, Python 3).
FROM ubuntu:24.04@sha256:a853f94d226358a79c740cfc7bce0c289748f3fe3488d921d038ccd752c61b60

ARG SOLDR_VERSION=0.9.28
ARG SOLDR_SHA256=00c33d9f0447c3762868f244bad5e6af78a4f9ab848069069450991a743b107f

# ZCCACHE_TEST_ISOLATED is the marker ci/nextest_isolation_guard.sh accepts
# in place of CI=true. Only isolated environments set it.
ENV ZCCACHE_TEST_ISOLATED=1
ENV DEBIAN_FRONTEND=noninteractive \
    SOLDR_VERSION=${SOLDR_VERSION}

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        clang \
        cmake \
        curl \
        git \
        libssl-dev \
        ninja-build \
        pkg-config \
        python3 \
        zstd \
 && rm -rf /var/lib/apt/lists/*

RUN curl --fail --location --silent --show-error \
        "https://github.com/zackees/soldr/releases/download/v${SOLDR_VERSION}/soldr-v${SOLDR_VERSION}-x86_64-unknown-linux-gnu.tar.zst" \
        -o /tmp/soldr.tar.zst \
 && echo "${SOLDR_SHA256}  /tmp/soldr.tar.zst" | sha256sum --check - \
 && mkdir /tmp/soldr \
 && tar --zstd -xf /tmp/soldr.tar.zst -C /tmp/soldr \
 && install -m 0755 /tmp/soldr/soldr /tmp/soldr/soldr-daemon /usr/local/bin/ \
 && rm -rf /tmp/soldr /tmp/soldr.tar.zst

# Run as an unprivileged user, like the hosted runner's `runner`. As root,
# read-only cache files stay writable (root bypasses permission checks), and
# the strict artifact-layout validation's warm link misses: tests that pin
# read-only materialization semantics only hold for a non-root user.
RUN useradd --create-home --uid 1001 gate \
 && mkdir -p /target /home/gate/.soldr \
 && chown gate:gate /target /home/gate/.soldr
USER gate
ENV HOME=/home/gate

WORKDIR /work

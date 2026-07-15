# syntax=docker/dockerfile:1.7

FROM emscripten/emsdk:3.1.74@sha256:af45409f3199d88db4b1b03af0098532c8fb33a375ac257463eeb0a622870d06

ARG RUST_VERSION=1.94.1
ARG CLANG_MAJOR=14
ARG SCCACHE_VERSION=0.10.0
ARG SCCACHE_SHA256=1fbb35e135660d04a2d5e42b59c7874d39b3deb17de56330b25b713ec59f849b
ARG SOLDR_VERSION=0.8.16
ARG SOLDR_SHA256=95e338a6a1dd941c248f95148557c434ce735392cba867007b7997d531200830

ENV DEBIAN_FRONTEND=noninteractive \
    CARGO_TARGET_DIR=/target \
    RUST_VERSION=${RUST_VERSION} \
    SOLDR_COMMAND_OUTPUT_TIMEOUT_SECS=600 \
    PATH=/emsdk:/emsdk/upstream/emscripten:/usr/local/bin:${PATH}

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        clang-14 \
        curl \
        git \
        jq \
        libssl-dev \
        llvm-14 \
        pkg-config \
        procps \
        time \
        zstd \
 && rm -rf /var/lib/apt/lists/* \
 && ln -sf /usr/bin/clang-${CLANG_MAJOR} /usr/local/bin/clang \
 && ln -sf /usr/bin/clang++-${CLANG_MAJOR} /usr/local/bin/clang++ \
 && ln -sf /usr/bin/llvm-ar-${CLANG_MAJOR} /usr/local/bin/llvm-ar

RUN curl --fail --location --silent --show-error \
        "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
        -o /tmp/sccache.tar.gz \
 && echo "${SCCACHE_SHA256}  /tmp/sccache.tar.gz" | sha256sum --check - \
 && tar -xzf /tmp/sccache.tar.gz -C /tmp \
 && install -m 0755 \
        "/tmp/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl/sccache" \
        /usr/local/bin/sccache \
 && rm -rf /tmp/sccache*

RUN curl --fail --location --silent --show-error \
        "https://github.com/zackees/soldr/releases/download/v${SOLDR_VERSION}/soldr-v${SOLDR_VERSION}-x86_64-unknown-linux-musl.tar.zst" \
        -o /tmp/soldr.tar.zst \
 && echo "${SOLDR_SHA256}  /tmp/soldr.tar.zst" | sha256sum --check - \
 && mkdir /tmp/soldr \
 && tar --zstd -xf /tmp/soldr.tar.zst -C /tmp/soldr \
 && install -m 0755 "$(find /tmp/soldr -type f -name soldr -print -quit)" /usr/local/bin/soldr \
 && rm -rf /tmp/soldr /tmp/soldr.tar.zst

# The musl soldr release runs on this older glibc base. Tell its managed
# rustup to install the native GNU host toolchain, then cache that complete
# soldr-owned state in an image layer for runtime volume seeding.
COPY rust-toolchain.toml /tmp/soldr-bootstrap/rust-toolchain.toml
RUN HOME=/opt/soldr-seed soldr bootstrap \
 && HOME=/opt/soldr-seed soldr rustup set default-host x86_64-unknown-linux-gnu \
 && cd /tmp/soldr-bootstrap \
 && HOME=/opt/soldr-seed soldr toolchain prepare \
 && HOME=/opt/soldr-seed soldr cargo --version \
 && HOME=/opt/soldr-seed soldr rustc --version \
 && rm -rf /tmp/soldr-bootstrap
ENV SOLDR_VERSION=${SOLDR_VERSION}
RUN touch "/opt/soldr-seed/.standalone-toolchain-${SOLDR_VERSION}-${RUST_VERSION}"

COPY --from=ghcr.io/astral-sh/uv:0.8.14 /uv /usr/local/bin/uv
COPY ci/docker/standalone_perf_entrypoint.sh /usr/local/bin/standalone-perf
RUN chmod 0755 /usr/local/bin/standalone-perf

ARG CAMPAIGN_RECIPE_SHA=unknown
LABEL org.zccache.campaign.rust="${RUST_VERSION}" \
      org.zccache.campaign.clang="${CLANG_MAJOR}" \
      org.zccache.campaign.sccache="${SCCACHE_VERSION}" \
      org.zccache.campaign.soldr="${SOLDR_VERSION}" \
      org.zccache.campaign.emsdk="3.1.74" \
      org.zccache.campaign.recipe="${CAMPAIGN_RECIPE_SHA}"

WORKDIR /src
ENTRYPOINT ["/usr/local/bin/standalone-perf"]

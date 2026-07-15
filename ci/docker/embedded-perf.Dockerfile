# syntax=docker/dockerfile:1.7

# Reuse the compiler-complete, soldr-prepared image from the standalone
# campaign. The locally-built soldr binary is mounted over /usr/local/bin/soldr
# at runtime so it contains the exact zccache commit under test.
FROM zccache-standalone-perf:1

COPY ci/docker/embedded_perf_entrypoint.sh /usr/local/bin/embedded-perf
RUN chmod 0755 /usr/local/bin/embedded-perf

ARG EMBEDDED_RECIPE_SHA=unknown
LABEL org.zccache.embedded-perf.recipe="${EMBEDDED_RECIPE_SHA}"

ENTRYPOINT ["/usr/local/bin/embedded-perf"]

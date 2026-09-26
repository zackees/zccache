#!/usr/bin/env bash
# Run the Linux x86_64 leg of .github/workflows/test-action.yml with act.
#
# Invoked by `bosn act-test-action` inside the bosn `act` stack. act talks to
# the host Docker engine, so its job containers are siblings outside bosn's
# label contract: every job container this run creates carries a unique
# `zccache.act-run` label, and the EXIT trap removes exactly those.
set -euo pipefail

cd /work

RUN_ID="act-test-action-$(date +%s)-$$"
RUNNER_IMAGE="${ACT_RUNNER_IMAGE:-catthehacker/ubuntu:act-24.04}"

cleanup() {
    local ids
    ids="$(docker ps -aq --filter "label=zccache.act-run=${RUN_ID}" || true)"
    if [ -n "${ids}" ]; then
        echo "[act-test-action] removing job containers for ${RUN_ID}: ${ids//$'\n'/ }"
        # shellcheck disable=SC2086
        docker rm -f ${ids} >/dev/null || true
    else
        echo "[act-test-action] no job containers left for ${RUN_ID}"
    fi
}
trap cleanup EXIT

# GITHUB_TOKEN is optional: the job needs no API access (public action clones,
# release download via the releases/latest redirect). Forward it only when the
# container environment already has it; never write a token to disk.
secret_args=()
if [ -n "${GITHUB_TOKEN:-}" ]; then
    secret_args=(-s GITHUB_TOKEN)
fi

echo "[act-test-action] run id ${RUN_ID}, runner image ${RUNNER_IMAGE}"
act --version

act pull_request \
    -W .github/workflows/test-action.yml \
    -j test-action \
    --matrix os:ubuntu-24.04 \
    --matrix target:x86_64-unknown-linux-gnu \
    -P "ubuntu-24.04=${RUNNER_IMAGE}" \
    --container-options "--label zccache.act-run=${RUN_ID}" \
    --rm \
    "${secret_args[@]}" \
    "$@"

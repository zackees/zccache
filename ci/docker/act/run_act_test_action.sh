#!/usr/bin/env bash
# Run the Linux x86_64 leg of .github/workflows/test-action.yml with act.
#
# Invoked by `bosn act-test-action` inside the bosn `act` stack. act talks to
# the host Docker engine, so its job containers are siblings outside bosn's
# label contract: every job container this run creates carries a unique
# `zccache.act-run` label, and the EXIT trap removes exactly those.
#
# Repeatable, cache-free reruns (#1777; the owner decided no local cache
# layer is checked in, see #1760):
#   - The job runs as a pull_request, as on origin/main: nothing is saved.
#   - act reads github.sha from git, and a worktree's .git points outside the
#     /work mount. The tree is snapshotted into a throwaway repo with a pinned
#     author/date, so the sha is a pure function of the file contents and the
#     cache keys advance exactly when the source does.
#   - act copies the workspace into the job container at the *same* absolute
#     path. The snapshot therefore lives at one fixed path, as the workspace
#     does on a GitHub runner; a per-run path turned every absolute path in a
#     compile into a new cache key (45 of 262 compiles missed after a restore).
#     A lock serializes concurrent runs on that path.
#   - The runner image and cached action checkouts are reused (--pull=false,
#     --action-offline-mode); both are fetched only when missing.
#
# Two local-only adaptations are applied to the snapshot *after* its sha is
# taken, so they never reach the cache keys or the real checkout:
#   - The cache pre-prune barrier becomes a no-op. It coordinates the GitHub
#     Actions cache through the GitHub API; under act the cache is the local
#     cache server, so there is nothing to wait for.
#   - Without GITHUB_TOKEN, setup-soldr cannot ask the GitHub API for the
#     latest soldr release (anonymous requests are rate-limited). The script
#     resolves that tag through the web `releases/latest` redirect instead and
#     passes it as setup-soldr's `version`.
set -euo pipefail

SRC=/work
RUN_ID="act-test-action-$(date +%s)-$$"
RUNNER_IMAGE="${ACT_RUNNER_IMAGE:-catthehacker/ubuntu:act-24.04}"
EVENT=pull_request
SCRATCH=/tmp/act-test-action
WORK="${SCRATCH}/src"
exec 9>/tmp/act-test-action.lock
if ! flock -n 9; then
    echo "[act-test-action] another run holds ${SCRATCH}; waiting for it"
    flock 9
fi
rm -rf "${SCRATCH}"
# Cached action checkouts (not an Actions cache) for --action-offline-mode.
ACTION_CACHE=/root/.cache/act

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
    rm -rf "${SCRATCH}"
}
trap cleanup EXIT

# Snapshot the working tree, uncommitted edits included. The excludes are the
# large ignored build/cache directories; act drops anything else .gitignore
# names when it copies the snapshot into the job container.
mkdir -p "${WORK}"
tar -C "${SRC}" \
    --exclude=./target --exclude=./.git --exclude=./.cargo --exclude=./.rustup \
    --exclude=./.perf-local --exclude=./.perf-standalone --exclude=./.venv \
    --exclude=./.zccache --exclude=./.cache --exclude=./.clud \
    --exclude=./.claude/worktrees --exclude=./_vender --exclude=./dist \
    --exclude=./zccache.egg-info --exclude=./tasks/bench-output \
    --exclude='./dylints/*/target' --exclude=__pycache__ \
    --exclude=.pytest_cache --exclude=.ruff_cache \
    -cf - . | tar -C "${WORK}" --no-same-owner -xf -
if [ -f "${SRC}/.cargo/config.toml" ]; then
    mkdir -p "${WORK}/.cargo"
    cp "${SRC}/.cargo/config.toml" "${WORK}/.cargo/config.toml"
fi

export GIT_AUTHOR_NAME=act GIT_AUTHOR_EMAIL=act@localhost
export GIT_COMMITTER_NAME=act GIT_COMMITTER_EMAIL=act@localhost
export GIT_AUTHOR_DATE="2000-01-01T00:00:00Z" GIT_COMMITTER_DATE="2000-01-01T00:00:00Z"
git -C "${WORK}" init -q -b main
git -C "${WORK}" add -A
git -C "${WORK}" commit -q --no-verify -m "act snapshot"
SHA="$(git -C "${WORK}" rev-parse HEAD)"

# --- local-only adaptations (after the sha) ---------------------------------
cat > "${WORK}/.github/workflows/wait-cache-pre-prune.yml" <<'EOF'
name: Wait for cache pre-prune (reusable)
on:
  workflow_call:
jobs:
  wait:
    name: Wait for exact-SHA cache pre-prune
    runs-on: ubuntu-latest
    steps:
      - run: echo "act - GitHub Actions cache pre-prune barrier skipped"
EOF
cat > "${WORK}/.github/actions/wait-cache-pre-prune/action.yml" <<'EOF'
name: Wait for exact-SHA cache pre-prune
description: act stand-in; the GitHub Actions cache is not used under act.
runs:
  using: composite
  steps:
    - run: echo "act - GitHub Actions cache pre-prune barrier skipped"
      shell: bash
EOF

if [ -z "${GITHUB_TOKEN:-}" ]; then
    soldr_tag="$(curl -fsSI https://github.com/zackees/soldr/releases/latest \
        | tr -d '\r' | sed -n 's@^[Ll]ocation: .*/releases/tag/\(.*\)$@\1@p')"
    if [ -z "${soldr_tag}" ]; then
        echo "[act-test-action] cannot resolve the latest soldr release tag" >&2
        exit 1
    fi
    awk -v tag="${soldr_tag}" '
        /uses: zackees\/setup-soldr@/ { pending = 1 }
        { print }
        pending && /^[[:space:]]*with:[[:space:]]*$/ {
            match($0, /^[[:space:]]*/)
            printf "%s  version: \"%s\"\n", substr($0, 1, RLENGTH), tag
            pending = 0
        }
    ' "${WORK}/.github/workflows/test-action.yml" > "${SCRATCH}/test-action.yml"
    mv "${SCRATCH}/test-action.yml" "${WORK}/.github/workflows/test-action.yml"
else
    soldr_tag="latest (GitHub API)"
fi

# GITHUB_TOKEN is optional (see the soldr adaptation above). Forward it only
# when the container environment already has it; never write a token to disk.
secret_args=()
if [ -n "${GITHUB_TOKEN:-}" ]; then
    secret_args=(-s GITHUB_TOKEN)
fi

echo "[act-test-action] run id ${RUN_ID}, event ${EVENT}, snapshot ${SHA}, soldr ${soldr_tag}, runner image ${RUNNER_IMAGE}"
act --version

cd "${WORK}"
act "${EVENT}" \
    -W .github/workflows/test-action.yml \
    -j test-action \
    --matrix os:ubuntu-24.04 \
    --matrix target:x86_64-unknown-linux-gnu \
    -P "ubuntu-24.04=${RUNNER_IMAGE}" \
    -P "ubuntu-latest=${RUNNER_IMAGE}" \
    --pull=false \
    --action-offline-mode \
    --action-cache-path "${ACTION_CACHE}" \
    --container-options "--label zccache.act-run=${RUN_ID}" \
    --rm \
    "${secret_args[@]}" \
    "$@"

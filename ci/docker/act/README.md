# Local GitHub Actions runs with act (bosn-managed)

The `act` stack in the repo-root [`bosn.toml`](../../../bosn.toml) runs this
repository's workflows locally with [nektos/act](https://github.com/nektos/act)
so a workflow change can be checked before it is pushed.

```bash
bosn ensure --stack act   # build the image (first run only)
bosn act-test-action      # Linux x86_64 leg of .github/workflows/test-action.yml
bosn status --json
bosn gc --dry-run --json
```

## What the stack contains

- `Dockerfile.bosn-act`: `debian:bookworm-slim` plus `act` v0.2.88 and the
  static Docker CLI, both downloaded from their official release URLs and
  checked against pinned SHA-256 digests (amd64 and arm64).
- `run_act_test_action.sh`: the `act-test-action` task. It runs
  `act pull_request -W .github/workflows/test-action.yml -j test-action`
  with `--matrix os:ubuntu-24.04 --matrix target:x86_64-unknown-linux-gnu`
  on `catthehacker/ubuntu:act-24.04`. Set `ACT_RUNNER_IMAGE` in the stack env
  to try another runner image. Extra arguments are passed through to `act`.

## Repeatable reruns

No local cache layer is checked in (#1777 owner decision; see #1760): the job
runs as a `pull_request`, and every setup-soldr cache input is off, so nothing is
saved between runs. What the runner does keep stable:

- **Content-addressed sha.** The tree, including uncommitted edits, is
  snapshotted into a throwaway git repo with a pinned author and date. That
  gives `github.sha` a value even from a worktree.
- **Fixed workspace path.** act copies the workspace into the job container
  at the same absolute path, so the snapshot always lives at
  `/tmp/act-test-action/src`, just as a GitHub runner's workspace path never
  changes. A per-run path made absolute compile paths differ between runs
  (45 of 262 compiles missed after a restore). A lock serializes concurrent
  runs.
- **No refetching.** `--pull=false` and `--action-offline-mode` reuse the
  local runner image and cached action checkouts; each is fetched only when
  missing. Pull the image by hand to pick up a new `act-24.04` tag.
- **soldr toolchain.** Rust comes from setup-soldr (see the root `CLAUDE.md`
  rule). Its standalone toolchain archive stays off (#1677), so each run
  installs the pinned toolchain with rustup. Do not re-enable that archive
  to make local runs faster: act's cache server has no 10 GB budget, so a
  local speedup here can hide a GitHub cache regression (#1760).
- **Local-only snapshot adaptations**, applied after the sha is taken so
  they never change cache keys or the real checkout:
  - the cache pre-prune barrier (`wait-cache-pre-prune`) becomes a no-op. It
    coordinates the GitHub Actions cache through the GitHub API, which act
    replaces with its local cache server;
  - without `GITHUB_TOKEN`, setup-soldr's `version` is pinned to the latest
    soldr tag, resolved through the web `releases/latest` redirect. Once that
    version's soldr is in setup-soldr's cache, warm runs make no GitHub API
    calls.

## Mounts and volumes

| Name          | Kind                 | Destination            | Why |
|---------------|----------------------|------------------------|-----|
| `repo`        | bind (`.`)           | `/work`                | act copies the checkout into each job container. |
| `docker-sock` | bind (`/var/run/docker.sock`) | same path     | act creates job containers on the host engine. |
| `act-cache`   | machine-scoped volume | `/root/.cache`        | act's action checkouts (`act/`) and cache-server data (`actcache/`) stay warm across runs. |

## Job containers are outside bosn's label contract

act's job containers are siblings on the host engine, so bosn does not label
or collect them. The runner script tags every job container with a unique
`zccache.act-run=<run id>` label (via `--container-options`), passes `--rm`,
and its `EXIT` trap removes the containers carrying that label. It never
touches containers from other act runs on the same host. act's shared
`act-toolcache` volume is left in place, as act expects.

## Limits

- act runs Linux containers only. The macOS, Windows and `ubuntu-24.04-arm`
  legs of `test-action` can only be verified on GitHub-hosted runners.
- `bosn run` does not forward the caller's environment into `docker exec`,
  and a token must never be written into `bosn.toml` or another file, so runs
  are anonymous by default. setup-soldr still needs the GitHub API once per
  soldr version, to fetch the release it then caches. Anonymous API requests
  share a 60/hour per-IP quota, so a cold seed run can fail with HTTP 403 on
  a busy host; rerun after the quota resets. The script forwards
  `-s GITHUB_TOKEN` only when the variable is already set inside the container.
- act short-circuits `actions/checkout` to the copied snapshot, so the job
  never fetches from GitHub. The snapshot excludes large ignored directories
  (`target/`, `.cargo/` except `config.toml`, `.perf-*`, `.venv`, ...).

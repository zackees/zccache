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
- `GITHUB_TOKEN` is optional. `bosn run` does not forward the caller's
  environment into `docker exec`, and a token must never be written into
  `bosn.toml` or another file, so runs are anonymous by default. The
  `test-action` job needs no API access: actions are cloned anonymously and
  the zccache install resolves `latest` through the `releases/latest` redirect.
  The script forwards `-s GITHUB_TOKEN` only when the variable is already set
  inside the container.
- The checkout at `/work` may be a git worktree whose `.git` file points
  outside the mount. act then logs that it cannot read git metadata and
  continues. The job does not depend on it: act short-circuits
  `actions/checkout` to the copied working tree.

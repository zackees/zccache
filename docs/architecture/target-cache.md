# Target Cache and Action Snapshot Ownership

This document defines the ownership boundary for the optional target snapshot
cache layer used by the composite GitHub Action.

## Strategic Owner

For soldr and setup-soldr integration, `zccache rust-plan` is the strategic
target artifact interface. It consumes a structured Rust build plan, restores
or saves artifacts through local or GitHub Actions backends, and emits
machine-readable summaries for setup-soldr.

Target snapshots are not the soldr integration interface. They are a legacy,
action-only compatibility layer for workflows that set the `cache-target` input
to `true` on the zackees/zccache action.

## Legacy Action Path

The legacy path is intentionally scoped to the composite action:

- `action.yml` restores the optional target snapshot and runs `zccache warm`.
- `action/cleanup/action.yml` records the fingerprint sidecar, invokes
  `prepare-target-snapshot.sh`, and saves the resulting `target-meta.tar`.
- `action/cleanup/prepare-target-snapshot.sh` owns snapshot mode selection,
  pruning, size parsing, size limits, tar creation, GitHub output fields, and
  step-summary text.
- `action/cleanup/select-hot-target.py` owns hot-file selection for the legacy
  hot snapshot mode.
- `zccache snapshot-bytes`, `snapshot-fp-record`, and `snapshot-fp-validate`
  are support commands for this legacy action flow.

This code exists to preserve action compatibility. New soldr/setup-soldr work
should not add target artifact behavior here unless the goal is explicitly to
replace the legacy action path.

## Output Contract

The cleanup action continues to expose these target snapshot outputs:

- `target-snapshot-saved`
- `target-snapshot-skipped-reason`
- `target-snapshot-bytes`
- `target-snapshot-candidate-bytes`
- `target-pruned-dirs`
- `target-pruned-bytes`

If a future native `zccache target-cache prepare --json` command replaces the
shell/Python path, it must preserve those fields and the current skip reasons:

- `missing-target-dir`
- `target-too-large`
- `no-hot-target-files`
- `tar-failed`

It must also preserve the current target snapshot modes:

- `hot`: save Cargo metadata plus files read or modified after setup.
- `full`: save the pruned target tree.

## Rust-Plan Relationship

The rust-plan path and the target snapshot path should remain separate:

- rust-plan is plan-driven and safe for soldr/setup-soldr ownership.
- target snapshots are action-input-driven and cache `target/` freshness state
  as a unit.
- rust-plan summaries are the vocabulary for soldr-facing reporting.
- target snapshot summaries are GitHub Action compatibility output.

When fixing a target artifact bug, use this rule:

- If the bug affects soldr/setup-soldr restore/save behavior, fix rust-plan.
- If the bug affects `cache-target: true` in the zackees/zccache action, fix
  the legacy target snapshot path.
- If both are affected, file separate issues unless the fix is strictly shared
  helper code.

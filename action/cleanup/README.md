# zccache cleanup action

Companion to the main `zackees/zccache` action. Stops the daemon and saves caches.

## Usage

Always call with `if: always()`:

```yaml
- if: always()
  uses: zackees/zccache/action/cleanup@v1
```

## What it does

1. Prints zccache stats (hit/miss rates)
2. Stops the zccache daemon
3. Saves compilation cache to GHA cache
4. Saves cargo registry cache to GHA cache
5. If `cache-target: true`, prunes and bounds the cargo target snapshot before saving
6. Cleans up temporary state files

Cache keys are read from state written by the setup action (`~/.zccache-action-state/`).

## Target Snapshot Outputs

When target snapshots are enabled, cleanup exposes:

| Output | Description |
|---|---|
| `target-snapshot-saved` | Whether a target snapshot tarball was created for saving |
| `target-snapshot-skipped-reason` | Why target snapshot creation was skipped |
| `target-snapshot-bytes` | Size in bytes of the target snapshot tarball |
| `target-snapshot-candidate-bytes` | Estimated payload bytes after pruning |
| `target-pruned-dirs` | Number of pruned target directories |
| `target-pruned-bytes` | Estimated bytes removed before snapshot creation |

//! Rebuild a lost artifact index from the payloads that survived on disk.
//!
//! # Why
//!
//! `index.bin` is a single bincode blob. Before #1157, any deserialize error
//! dropped the entire in-memory index and the daemon started empty — while
//! every content-addressed payload was still sitting in the artifact
//! directory, fully intact. One bad byte therefore cost a full cold recompile
//! of every workspace on the machine, with no local cause an operator could
//! see. This module is the reconciliation step: given the artifact directory,
//! reconstruct as many index entries as can be reconstructed *safely*.
//!
//! # What can and cannot be reconstructed
//!
//! [`ArtifactIndex`] carries seven fields. Disk provides four of them:
//!
//! | field | recoverable? | from |
//! |---|---|---|
//! | `output_sizes` | yes | the `.staged-v2` manifest, blake3-verified against the payload bytes |
//! | `total_size` | yes | sum of the above |
//! | `stored_at_secs` | yes | the manifest's mtime (stable across restarts, so reconciliation is idempotent) |
//! | `exit_code` | yes, as `0` | only successful compiles are ever stored (see `handle_compile::pipeline::store_outcome`) |
//! | `stdout` / `stderr` | **no** | never written to disk beside the payloads |
//! | `output_names` | **no** | the manifest records `index`, `size`, `digest` — never a filename |
//!
//! Empty `stdout`/`stderr` is a sanctioned degradation: a reconciled hit
//! replays without the original compiler warnings. That loses diagnostics, not
//! correctness.
//!
//! # Why single-output generations only
//!
//! The missing `output_names` is the reason this module reconstructs *less*
//! than it could. For a single-output artifact the name is never consulted
//! when placing the payload — the daemon writes payload 0 to the output path
//! the *current* request asked for. For multi-output artifacts the name is
//! load-bearing in three places, and guessing it produces a wrong cache hit,
//! which is catastrophic where a miss is merely slow:
//!
//! * `handle_compile::cached_hit` places outputs at index >= 1 at
//!   `secondary_output_dir.join(&names[i])`.
//! * the rustc `--emit` compatibility path picks *which* payload satisfies
//!   *which* requested output by matching names (and, failing that, by
//!   matching the name's extension class).
//! * `handle_exec` pairs declared outputs to payloads by exact name.
//!
//! So: generations with more than one output are counted and skipped.
//!
//! The synthetic name given to the surviving single output is deliberately
//! chosen to match *nothing*: [`RECONCILED_OUTPUT_NAME`] carries an extension
//! outside the rustc output-kind table, so the extension-class fallback in the
//! `--emit` compat path returns `None` and the request takes a clean miss
//! rather than receiving, say, an `.rmeta` payload delivered as an `.rlib`.
//!
//! # Why staged-v2 only
//!
//! Flat-v1 (`<key>_<i>`) and pack (`<key>.pack`) layouts are deliberately not
//! reconciled. Neither records an authoritative output *count*: a flat group
//! whose `_1` was removed is indistinguishable from a genuine single-output
//! artifact, and reconstructing it as single-output would serve a truncated
//! artifact as a complete hit. The staged manifest states the count outright,
//! which is exactly the property reconciliation needs.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::store::ArtifactIndex;

/// Filename recorded for the single output of a reconciled entry.
///
/// Never a real compiler output name, and specifically chosen so that
/// `handle_compile::cached_hit::rustc_output_kind` classifies it as `None`:
/// the `--emit` compatibility path then refuses to map any requested output
/// onto this payload and the request misses instead of receiving the wrong
/// bytes under the right filename.
pub const RECONCILED_OUTPUT_NAME: &str = "zccache-reconciled-output.zcunknown";

/// Default wall-clock cap on a reconciliation scan.
///
/// Reconciliation runs on the daemon startup path, and the scan cost is
/// `O(total cached bytes)` because every payload is blake3-verified. On a very
/// large cache that is unbounded, so the scan stops at the budget and the
/// daemon starts with whatever it recovered so far. Partial recovery beats a
/// daemon that will not come up.
pub const DEFAULT_RECONCILE_BUDGET: Duration = Duration::from_secs(5);

/// Result of one reconciliation scan. Every field is reported in the
/// `index_reconciled` lifecycle event so a partial recovery is attributable.
#[derive(Debug, Default)]
pub struct IndexReconciliation {
    /// Rebuilt `(key, entry)` rows, ready to insert into an `ArtifactStore`.
    pub entries: Vec<(String, ArtifactIndex)>,
    /// Published staged generations found on disk (the candidate set).
    pub candidates: usize,
    /// Generations skipped because their output filenames are unrecoverable.
    pub skipped_multi_output: usize,
    /// Generations skipped because a manifest or payload failed verification.
    pub skipped_unverifiable: usize,
    /// Whether the scan stopped at the budget with candidates left unvisited.
    pub truncated_by_budget: bool,
    /// Wall-clock cost of the scan.
    pub elapsed_ns: u64,
}

impl IndexReconciliation {
    /// Number of entries recovered.
    pub fn recovered(&self) -> usize {
        self.entries.len()
    }
}

/// Rebuild index entries from the staged-v2 generations under `artifact_dir`.
///
/// Never fails: an unreadable staged root, an unreadable manifest, and a
/// payload whose digest does not match are all counted and skipped, because
/// the caller's alternative is an empty index either way.
///
/// `budget` is checked before each candidate, so `Duration::ZERO` returns
/// immediately with `truncated_by_budget` set — which is how the budget
/// behaviour is tested without sleeping.
pub fn reconcile_index_from_disk(artifact_dir: &Path, budget: Duration) -> IndexReconciliation {
    let started = Instant::now();
    let mut outcome = IndexReconciliation::default();

    let keys = match crate::layout::published_staged_keys(artifact_dir) {
        Ok(keys) => keys,
        Err(error) => {
            tracing::warn!(
                artifact_dir = %artifact_dir.display(),
                "artifact index reconciliation could not list staged generations: {error}"
            );
            outcome.elapsed_ns = started.elapsed().as_nanos() as u64;
            return outcome;
        }
    };
    outcome.candidates = keys.len();

    for key in keys {
        if started.elapsed() >= budget {
            outcome.truncated_by_budget = true;
            break;
        }
        match crate::layout::verified_staged_generation(artifact_dir, &key) {
            Ok(Some((sizes, stored_at))) => {
                if sizes.len() != 1 {
                    outcome.skipped_multi_output += 1;
                    continue;
                }
                outcome.entries.push((key, rebuilt_entry(sizes, stored_at)));
            }
            // The pointer vanished between listing and reading (a concurrent
            // publish or eviction). Not a corruption signal.
            Ok(None) => outcome.skipped_unverifiable += 1,
            Err(error) => {
                tracing::debug!(
                    %key,
                    "artifact index reconciliation skipped an unverifiable generation: {error}"
                );
                outcome.skipped_unverifiable += 1;
            }
        }
    }

    outcome.elapsed_ns = started.elapsed().as_nanos() as u64;
    outcome
}

/// Build the conservative index entry for one verified single-output
/// generation: real sizes, a synthetic non-matching name, no captured
/// output, and the success exit code that is the only one ever stored.
fn rebuilt_entry(sizes: Vec<u64>, stored_at: SystemTime) -> ArtifactIndex {
    let total_size = sizes.iter().copied().sum();
    ArtifactIndex {
        output_names: Arc::from(vec![RECONCILED_OUTPUT_NAME.to_string()]),
        output_sizes: sizes,
        stdout: Arc::new(Vec::new()),
        stderr: Arc::new(Vec::new()),
        exit_code: 0,
        total_size,
        stored_at_secs: stored_at
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        rustc_verdicts: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::resolve_artifact_payloads;

    #[test]
    fn a_missing_staged_root_reconciles_to_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = reconcile_index_from_disk(dir.path(), DEFAULT_RECONCILE_BUDGET);
        assert_eq!(outcome.recovered(), 0);
        assert_eq!(outcome.candidates, 0);
        assert!(!outcome.truncated_by_budget);
    }

    #[test]
    fn a_zero_budget_recovers_nothing_and_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = "a".repeat(64);
        crate::layout::fixtures::seed_staged_generation(dir.path(), &key, &[b"payload"]);

        let outcome = reconcile_index_from_disk(dir.path(), Duration::ZERO);
        assert_eq!(outcome.candidates, 1, "the candidate was still discovered");
        assert_eq!(outcome.recovered(), 0);
        assert!(
            outcome.truncated_by_budget,
            "an exhausted budget must be reported, not silently look like an empty cache"
        );
    }

    #[test]
    fn a_single_output_generation_is_rebuilt_and_re_resolves() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = "b".repeat(64);
        crate::layout::fixtures::seed_staged_generation(dir.path(), &key, &[b"object-bytes"]);

        let outcome = reconcile_index_from_disk(dir.path(), DEFAULT_RECONCILE_BUDGET);
        assert_eq!(outcome.recovered(), 1);
        let (rebuilt_key, entry) = &outcome.entries[0];
        assert_eq!(rebuilt_key, &key);
        assert_eq!(entry.output_sizes, vec![b"object-bytes".len() as u64]);
        assert_eq!(entry.total_size, b"object-bytes".len() as u64);
        assert_eq!(entry.exit_code, 0);
        assert!(entry.stdout.is_empty() && entry.stderr.is_empty());
        assert_eq!(&*entry.output_names, &[RECONCILED_OUTPUT_NAME.to_string()]);

        // The whole point: the rebuilt sizes must satisfy the same resolver
        // the hit path uses, so the entry is actually re-hittable.
        let payloads =
            resolve_artifact_payloads(dir.path(), &key, &entry.output_sizes, true, "test::rebuilt")
                .expect("resolve")
                .expect("rebuilt entry must resolve to its payload");
        assert_eq!(payloads.len(), 1);
    }

    #[test]
    fn a_multi_output_generation_is_skipped_rather_than_guessed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = "c".repeat(64);
        crate::layout::fixtures::seed_staged_generation(dir.path(), &key, &[b"object", b"depfile"]);

        let outcome = reconcile_index_from_disk(dir.path(), DEFAULT_RECONCILE_BUDGET);
        assert_eq!(
            outcome.recovered(),
            0,
            "output filenames are unrecoverable, and guessing them is a wrong hit"
        );
        assert_eq!(outcome.skipped_multi_output, 1);
    }

    #[test]
    fn a_tampered_payload_is_skipped_not_reconstructed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = "d".repeat(64);
        let generation =
            crate::layout::fixtures::seed_staged_generation(dir.path(), &key, &[b"original"]);
        std::fs::write(
            dir.path()
                .join(".staged-v2")
                .join(&key)
                .join(&generation)
                .join("output-0"),
            b"tampered",
        )
        .expect("tamper");

        let outcome = reconcile_index_from_disk(dir.path(), DEFAULT_RECONCILE_BUDGET);
        assert_eq!(outcome.recovered(), 0);
        assert_eq!(outcome.skipped_unverifiable, 1);
    }

    #[test]
    fn reconciliation_is_idempotent_across_repeated_scans() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (index, byte) in [b'1', b'2', b'3'].into_iter().enumerate() {
            let key = (byte as char).to_string().repeat(64);
            crate::layout::fixtures::seed_staged_generation(
                dir.path(),
                &key,
                &[format!("payload-{index}").as_bytes()],
            );
        }

        let first = reconcile_index_from_disk(dir.path(), DEFAULT_RECONCILE_BUDGET);
        let second = reconcile_index_from_disk(dir.path(), DEFAULT_RECONCILE_BUDGET);
        assert_eq!(first.recovered(), 3);
        assert_eq!(second.recovered(), 3);

        let keys = |outcome: &IndexReconciliation| -> Vec<String> {
            outcome.entries.iter().map(|(k, _)| k.clone()).collect()
        };
        assert_eq!(keys(&first), keys(&second));
        for ((_, a), (_, b)) in first.entries.iter().zip(second.entries.iter()) {
            assert_eq!(a.output_sizes, b.output_sizes);
            assert_eq!(
                a.stored_at_secs, b.stored_at_secs,
                "stored_at must come from the manifest mtime, not now(), or every \
                 restart would reset retention age for the whole cache"
            );
        }
    }
}

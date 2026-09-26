//! Layer D of the #1683 test design: `ZCCACHE_MODE` against the real
//! cache-hit executor on the test volume. COPY and REFLINK share one
//! delivery contract ([`assert_independent_delivery`]); LINK and AUTO may
//! share the cache inode only for outputs whose policy allows it.

use super::super::*;
use crate::compiler::DeliveryPolicy;
use MaterializationMode::{Auto, Copy, Link, Reflink};

const BYTES: &[u8] = b"immutable cached rust archive";

struct Fixture {
    dir: tempfile::TempDir,
    cache: PathBuf,
}

impl Fixture {
    /// A legacy (non-staged) cache blob with a registered digest and an old
    /// mtime, so mtime restoration is observable.
    fn legacy() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cached.rlib");
        std::fs::write(&cache, BYTES).unwrap();
        write_authoritative_blob_digest(&cache).unwrap();
        let old = kernal_api::platform::fs::FileTime::from_unix_time(1_000_000_000, 0);
        kernal_api::platform::fs::set_file_mtime(&cache, old).unwrap();
        Self { dir, cache }
    }

    /// A staged generation blob; its delivery policy is what the caller
    /// passes (staged outputs are only shared when the policy allows it).
    fn staged() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let artifact_dir = dir.path().join("artifacts");
        std::fs::create_dir_all(&artifact_dir).unwrap();
        let source = dir.path().join("source.rlib");
        std::fs::write(&source, BYTES).unwrap();
        let key = "c".repeat(64);
        persist_staged_artifact_paths(&artifact_dir, &key, &[source.into()]).unwrap();
        let payloads = load_staged_artifact_paths(&artifact_dir, &key, &[BYTES.len() as u64])
            .unwrap()
            .unwrap();
        let cache = payloads[0].as_path().to_path_buf();
        Self { dir, cache }
    }

    fn out(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn deliver(
        &self,
        out: &Path,
        policy: DeliveryPolicy,
        mode: MaterializationMode,
    ) -> StagedMaterializationStats {
        materialize_cached_file_with_mode(out, &self.cache, policy, mode, true).unwrap()
    }

    fn hardlinks_supported(&self) -> bool {
        fs_caps_raw(&self.cache, &self.out("probe-target")).hardlink
    }
}

fn mtime(path: &Path) -> kernal_api::platform::fs::FileTime {
    kernal_api::platform::fs::FileTime::from_last_modification_time(
        &std::fs::metadata(path).unwrap(),
    )
}

fn same_file(a: &Path, b: &Path) -> bool {
    crate::platform::fs::identity::same_file(a, b).unwrap()
}

/// The COPY/REFLINK contract: an independent, writable inode with the cache
/// file's mtime, whose edits never reach the cache.
fn assert_independent_delivery(out: &Path, cache: &Path, cache_mtime_seconds: i64) {
    assert!(!same_file(out, cache), "output shares the cache inode");
    assert_eq!(
        crate::platform::fs::links::hard_link_count(out).unwrap(),
        1,
        "an independent output has exactly one link"
    );
    assert!(
        !std::fs::metadata(out).unwrap().permissions().readonly(),
        "an independent output must be writable"
    );
    assert_eq!(mtime(out).unix_seconds(), cache_mtime_seconds);
    assert_eq!(std::fs::read(out).unwrap(), BYTES);
    std::fs::write(out, b"edited in place by a later tool").unwrap();
    assert_eq!(
        std::fs::read(cache).unwrap(),
        BYTES,
        "edit reached the cache"
    );
}

fn assert_shared_delivery(out: &Path, cache: &Path) {
    assert!(same_file(out, cache), "output must share the cache inode");
    assert!(crate::platform::fs::links::hard_link_count(cache).unwrap() >= 2);
}

#[test]
fn copy_mode_delivers_an_independent_copy_without_probing() {
    let fixture = Fixture::legacy();
    let cache_mtime = mtime(&fixture.cache).unix_seconds();
    let out = fixture.out("copy.rlib");
    let observed = fixture.deliver(&out, DeliveryPolicy::HardlinkEligible, Copy);
    assert_eq!(
        (
            observed.reflink_count,
            observed.hardlink_count,
            observed.copy_count
        ),
        (0, 0, 1)
    );
    assert_eq!(observed.copy_bytes, BYTES.len() as u64);
    assert_eq!(probes_under(fixture.dir.path()), 0, "COPY must not probe");
    assert_independent_delivery(&out, &fixture.cache, cache_mtime);
}

#[test]
fn reflink_mode_delivers_an_independent_file_never_a_hardlink() {
    let fixture = Fixture::legacy();
    let cache_mtime = mtime(&fixture.cache).unix_seconds();
    let out = fixture.out("reflink.rlib");
    let observed = fixture.deliver(&out, DeliveryPolicy::HardlinkEligible, Reflink);
    assert_eq!(observed.hardlink_count, 0);
    assert_eq!(observed.reflink_count + observed.copy_count, 1);
    assert_independent_delivery(&out, &fixture.cache, cache_mtime);
}

#[test]
fn reflink_mode_falls_back_to_copy_when_the_clone_fails() {
    let fixture = Fixture::legacy();
    let cache_mtime = mtime(&fixture.cache).unix_seconds();
    let out = fixture.out("fallback.rlib");
    let faults = StagedFaultGuard::arm(&out, [StagedFaultPoint::MaterializeReflink]);
    let observed = fixture.deliver(&out, DeliveryPolicy::HardlinkEligible, Reflink);
    faults.assert_all_consumed();
    assert_eq!(
        (
            observed.reflink_count,
            observed.hardlink_count,
            observed.copy_count
        ),
        (0, 0, 1)
    );
    assert_independent_delivery(&out, &fixture.cache, cache_mtime);
}

#[test]
fn link_mode_hardlinks_an_eligible_output() {
    let fixture = Fixture::legacy();
    if !fixture.hardlinks_supported() {
        eprintln!("SKIP link_mode_hardlinks_an_eligible_output: no hardlinks on this volume");
        return;
    }
    let out = fixture.out("linked.rlib");
    let observed = fixture.deliver(&out, DeliveryPolicy::HardlinkEligible, Link);
    assert_eq!(
        (
            observed.reflink_count,
            observed.hardlink_count,
            observed.copy_count
        ),
        (0, 1, 0),
        "LINK never clones an output it may link"
    );
    assert_shared_delivery(&out, &fixture.cache);
}

#[test]
fn link_mode_demotes_an_independent_only_output() {
    let fixture = Fixture::staged();
    let cache_mtime = mtime(&fixture.cache).unix_seconds();
    let out = fixture.out("independent.rlib");
    let observed = fixture.deliver(&out, DeliveryPolicy::IndependentOnly, Link);
    assert_eq!(observed.hardlink_count, 0);
    assert_eq!(observed.reflink_count + observed.copy_count, 1);
    assert_independent_delivery(&out, &fixture.cache, cache_mtime);
}

#[test]
fn link_mode_falls_back_to_copy_when_the_hardlink_fails() {
    let fixture = Fixture::legacy();
    if !fixture.hardlinks_supported() {
        eprintln!("SKIP link_mode_falls_back_to_copy: no hardlinks on this volume");
        return;
    }
    let cache_mtime = mtime(&fixture.cache).unix_seconds();
    let out = fixture.out("link-fallback.rlib");
    let faults = StagedFaultGuard::arm(
        &out,
        [
            StagedFaultPoint::MaterializeReflink,
            StagedFaultPoint::MaterializeHardlink,
        ],
    );
    let observed = fixture.deliver(&out, DeliveryPolicy::HardlinkEligible, Link);
    faults.assert_all_consumed();
    assert_eq!(observed.copy_count, 1);
    assert_independent_delivery(&out, &fixture.cache, cache_mtime);
}

#[test]
fn copy_failure_is_a_clean_error_in_every_mode() {
    for mode in MaterializationMode::ALL {
        let fixture = Fixture::legacy();
        let out = fixture.out("failed.rlib");
        let _faults = StagedFaultGuard::arm(
            &out,
            [
                StagedFaultPoint::MaterializeReflink,
                StagedFaultPoint::MaterializeHardlink,
                StagedFaultPoint::MaterializeCopy,
            ],
        );
        let result = materialize_cached_file_with_mode(
            &out,
            &fixture.cache,
            DeliveryPolicy::HardlinkEligible,
            mode,
            true,
        );
        assert!(
            result.is_err(),
            "{mode}: an injected copy failure must surface"
        );
        assert!(!out.exists(), "{mode}: no partial output");
        assert_eq!(std::fs::read(&fixture.cache).unwrap(), BYTES, "{mode}");
    }
}

/// Switching from a sharing mode to an independent one migrates the
/// existing output on its next hit instead of leaving the shared inode.
#[test]
fn switching_link_to_an_independent_mode_detaches_the_existing_hardlink() {
    for independent in [Copy, Reflink] {
        let fixture = Fixture::legacy();
        if !fixture.hardlinks_supported() {
            eprintln!("SKIP switching_link_to_{independent}: no hardlinks on this volume");
            return;
        }
        let cache_mtime = mtime(&fixture.cache).unix_seconds();
        let out = fixture.out("migrated.rlib");
        fixture.deliver(&out, DeliveryPolicy::HardlinkEligible, Link);
        assert_shared_delivery(&out, &fixture.cache);
        let observed = fixture.deliver(&out, DeliveryPolicy::HardlinkEligible, independent);
        assert_eq!(observed.hardlink_count, 0, "{independent}");
        assert_eq!(
            crate::platform::fs::links::hard_link_count(&fixture.cache).unwrap(),
            1,
            "{independent}: the cache file must be the last link again"
        );
        assert_independent_delivery(&out, &fixture.cache, cache_mtime);
    }
}

#[test]
fn switching_copy_to_link_shares_the_cache_inode() {
    let fixture = Fixture::legacy();
    if !fixture.hardlinks_supported() {
        eprintln!("SKIP switching_copy_to_link: no hardlinks on this volume");
        return;
    }
    let out = fixture.out("relinked.rlib");
    fixture.deliver(&out, DeliveryPolicy::HardlinkEligible, Copy);
    assert!(!same_file(&out, &fixture.cache));
    fixture.deliver(&out, DeliveryPolicy::HardlinkEligible, Link);
    assert_shared_delivery(&out, &fixture.cache);
}

/// Capabilities are cached per volume pair independent of the mode, so
/// alternating modes never re-probes; COPY does not probe at all.
#[test]
fn capability_cache_is_mode_independent() {
    let fixture = Fixture::legacy();
    for (index, mode) in [Copy, Auto, Link, Reflink, Auto].into_iter().enumerate() {
        let out = fixture.out(&format!("alternating-{index}.rlib"));
        fixture.deliver(&out, DeliveryPolicy::IndependentOnly, mode);
    }
    assert_eq!(probes_under(fixture.dir.path()), 1);
}

/// `ZCCACHE_COW_READONLY` protects a *shared* blob only: independent
/// deliveries never flip the cache file's permissions, whichever way they
/// start.
#[test]
fn independent_modes_never_change_the_cache_file_permissions() {
    for mode in [Copy, Reflink] {
        for start_readonly in [false, true] {
            let fixture = Fixture::legacy();
            crate::platform::fs::permissions::set_readonly(&fixture.cache, start_readonly).unwrap();
            let out = fixture.out("perm.rlib");
            fixture.deliver(&out, DeliveryPolicy::HardlinkEligible, mode);
            assert_eq!(
                std::fs::metadata(&fixture.cache)
                    .unwrap()
                    .permissions()
                    .readonly(),
                start_readonly,
                "{mode}: cache permissions changed (started read-only: {start_readonly})"
            );
            assert!(
                !std::fs::metadata(&out).unwrap().permissions().readonly(),
                "{mode}"
            );
            let _ = crate::platform::fs::permissions::make_writable(&fixture.cache);
        }
    }
}

/// A mode switch that detaches a hardlinked output (LINK -> COPY) must
/// apply the sibling floor like any copy, or cargo sees the output older
/// than its siblings and recompiles dependents (#466/#467).
#[test]
fn mode_switch_detach_applies_the_sibling_floor() {
    let fixture = Fixture::legacy();
    if !fixture.hardlinks_supported() {
        eprintln!("SKIP mode_switch_detach_applies_the_sibling_floor: no hardlinks here");
        return;
    }
    let out = fixture.out("floored.rlib");
    fixture.deliver(&out, DeliveryPolicy::HardlinkEligible, Link);
    assert_shared_delivery(&out, &fixture.cache);
    let sibling = fixture.out("newer-sibling.rlib");
    std::fs::write(&sibling, b"sibling").unwrap();
    let newer = kernal_api::platform::fs::FileTime::from_unix_time(1_500_000_000, 0);
    kernal_api::platform::fs::set_file_mtime(&sibling, newer).unwrap();

    fixture.deliver(&out, DeliveryPolicy::HardlinkEligible, Copy);
    assert!(!same_file(&out, &fixture.cache));
    assert!(
        mtime(&out).unix_seconds() >= newer.unix_seconds(),
        "the detached output must be floored to its newest sibling"
    );
    assert_eq!(
        mtime(&fixture.cache).unix_seconds(),
        1_000_000_000,
        "flooring the output must not touch the cache file"
    );
}

/// The batch path used by compile, link, exec and multi-source hits honors
/// the mode for every target, parallel lane included.
#[test]
fn batch_delivery_honors_the_mode_for_every_target() {
    let fixture = Fixture::legacy();
    let targets: Vec<NormalizedPath> = (0..PAR_WRITE_THRESHOLD + 2)
        .map(|index| fixture.out(&format!("batch-{index}.rlib")).into())
        .collect();
    let payloads = vec![CachedPayload::File(fixture.cache.clone().into()); targets.len()];
    let policies = vec![DeliveryPolicy::HardlinkEligible; targets.len()];
    // Legacy (non-staged) blobs are not tier-observed on the batch path, so
    // the contract is asserted on the files themselves.
    write_payloads_par_with_mtime_floor_and_policies_observed(
        &targets,
        &payloads,
        &Vec::<NormalizedPath>::new(),
        &policies,
        Copy,
    )
    .unwrap();
    for target in &targets {
        assert!(!same_file(target, &fixture.cache));
        assert_eq!(std::fs::read(target).unwrap(), BYTES);
    }
    assert_eq!(
        crate::platform::fs::links::hard_link_count(&fixture.cache).unwrap(),
        1
    );
}

/// #1597/ETXTBSY: every mode holds the spawn exclusion while it opens the
/// requested output for writing.
#[cfg(unix)]
#[test]
fn every_mode_waits_for_child_spawn_before_writing() {
    use std::sync::mpsc;
    use std::time::Duration;

    for mode in MaterializationMode::ALL {
        let fixture = Fixture::legacy();
        let out = fixture.out("build-script-build");
        let cache = fixture.cache.clone();
        let spawn_guard = crate::daemon::spawn_exclusion::spawn_shared();
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let writer_out = out.clone();
        let writer = std::thread::spawn(move || {
            let result = materialize_cached_file_with_mode(
                &writer_out,
                &cache,
                DeliveryPolicy::IndependentOnly,
                mode,
                true,
            );
            done_tx.send(result.map(|_| ())).unwrap();
        });
        assert!(
            matches!(
                done_rx.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "{mode}: wrote while a child could inherit its descriptor"
        );
        drop(spawn_guard);
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("materialization did not resume after child exec")
            .unwrap();
        writer.join().unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), BYTES, "{mode}");
    }
}

/// `zccache status` counts every delivery by tier and every REFLINK -> copy
/// fallback. The counters are process-wide and other tests deliver in
/// parallel, so only a lower bound is asserted.
#[test]
fn deliveries_and_reflink_fallbacks_are_counted_for_status() {
    let before = materialization_status(None);
    let fixture = Fixture::legacy();
    fixture.deliver(
        &fixture.out("counted.rlib"),
        DeliveryPolicy::HardlinkEligible,
        Copy,
    );
    let fallback = fixture.out("fallback-counted.rlib");
    let faults = StagedFaultGuard::arm(&fallback, [StagedFaultPoint::MaterializeReflink]);
    fixture.deliver(&fallback, DeliveryPolicy::HardlinkEligible, Reflink);
    faults.assert_all_consumed();
    let after = materialization_status(Some(Copy));
    assert!(after.copy >= before.copy + 2, "{before:?} -> {after:?}");
    assert!(
        after.reflink_fallbacks > before.reflink_fallbacks,
        "{before:?} -> {after:?}"
    );
    assert_eq!(after.mode, "COPY");
    assert_eq!(materialization_status(None).mode, "AUTO");
}

//! `ZCCACHE_MODE` delivery planning (#1683).
//!
//! [`plan_tiers`] is the pure decision: given the selected mode, whether the
//! output's delivery policy allows sharing an inode, and the probed volume
//! capabilities, it returns which tiers the executor may try. The tier order
//! is always reflink -> hardlink -> copy; copy is the unconditional last
//! resort, so every hit delivers something.
//!
//! The mode is resolved per request ([`resolve_request_mode`]): the client's
//! forwarded `ZCCACHE_MODE`, else the service default (an embedded host's
//! environment at start or its setting; a standalone daemon has none), else
//! `AUTO`.

use super::*;
use crate::core::config::MaterializationMode;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

/// Which tiers a delivery may try, in the fixed order reflink -> hardlink;
/// a byte copy always follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::daemon::server) struct TierPlan {
    pub(in crate::daemon::server) reflink: bool,
    pub(in crate::daemon::server) hardlink: bool,
}

/// Whether `mode` may deliver by sharing the cache file's inode. The
/// output's delivery policy must allow it too: the mode only ever *demotes*
/// an output to independent delivery, never promotes one past its policy
/// (#1683 decision D1). Needs no capability probe, so the same-inode fast
/// path can consult it for free.
pub(in crate::daemon::server) const fn hardlink_permitted(
    mode: MaterializationMode,
    hardlink_eligible: bool,
) -> bool {
    hardlink_eligible && matches!(mode, MaterializationMode::Auto | MaterializationMode::Link)
}

/// The pure tier decision. `link_count` is the cache file's current hard-link
/// count; it only matters when a hardlink is permitted.
pub(in crate::daemon::server) fn plan_tiers(
    mode: MaterializationMode,
    hardlink_eligible: bool,
    caps: VolumeCaps,
    link_count: u64,
) -> TierPlan {
    let hardlink =
        hardlink_permitted(mode, hardlink_eligible) && hardlink_below_limit(caps, link_count);
    let reflink = caps.reflink
        && match mode {
            MaterializationMode::Auto | MaterializationMode::Reflink => true,
            // LINK never clones an output it may link; an output its policy
            // keeps independent takes the reflink-else-copy ladder.
            MaterializationMode::Link => !hardlink_eligible,
            MaterializationMode::Copy => false,
        };
    TierPlan { reflink, hardlink }
}

/// Tiers the *store* direction (compiler output -> cache blob) may try. The
/// store has no delivery policy of its own: AUTO and LINK keep today's
/// reflink -> hardlink -> copy order, but a hardlinked store leaves the build
/// output sharing the cache blob's inode, which COPY and REFLINK forbid.
pub(in crate::daemon::server) const fn plan_store_tiers(mode: MaterializationMode) -> TierPlan {
    let tiers = mode.tiers_for_shareable();
    TierPlan {
        reflink: tiers.reflink,
        hardlink: tiers.hardlink,
    }
}

/// Capabilities the executor plans against. `COPY` never probes the volume;
/// the legacy `ZCCACHE_DISABLE_REFLINK` switch applies to every mode except
/// an explicit `REFLINK` (#1683 decision D3).
pub(in crate::daemon::server) fn delivery_caps(
    mode: MaterializationMode,
    cache_file: &Path,
    out_path: &Path,
) -> VolumeCaps {
    if mode == MaterializationMode::Copy {
        return VolumeCaps::copy_only();
    }
    caps_for_mode(
        mode,
        fs_caps_raw(cache_file, out_path),
        legacy_reflink_disabled(),
    )
}

/// The pure half of [`delivery_caps`].
pub(in crate::daemon::server) fn caps_for_mode(
    mode: MaterializationMode,
    probed: VolumeCaps,
    legacy_reflink_disabled: bool,
) -> VolumeCaps {
    if mode == MaterializationMode::Copy {
        return VolumeCaps::copy_only();
    }
    apply_reflink_switch(
        probed,
        mode != MaterializationMode::Reflink && legacy_reflink_disabled,
    )
}

static REFLINK_FALLBACK_WARNED: AtomicBool = AtomicBool::new(false);

/// Outputs delivered per tier since process start: reflink, hardlink, copy,
/// and REFLINK -> copy fallbacks. Process-wide, like the capability cache.
static DELIVERED: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Count delivered outputs by tier for `zccache status` (#1683).
pub(in crate::daemon::server) fn record_delivery(reflink: u64, hardlink: u64, copy: u64) {
    for (slot, count) in DELIVERED.iter().zip([reflink, hardlink, copy]) {
        if count > 0 {
            slot.fetch_add(count, Ordering::Relaxed);
        }
    }
}

/// Status snapshot: the service-default mode and the per-tier counters.
pub(in crate::daemon::server) fn materialization_status(
    service_default: Option<MaterializationMode>,
) -> crate::protocol::MaterializationStatus {
    let [reflink, hardlink, copy, reflink_fallbacks] = DELIVERED
        .each_ref()
        .map(|slot| slot.load(Ordering::Relaxed));
    crate::protocol::MaterializationStatus {
        mode: service_default.unwrap_or_default().as_str().to_string(),
        reflink,
        hardlink,
        copy,
        reflink_fallbacks,
    }
}

/// `REFLINK` could not clone and delivered a copy instead (#1683 decision
/// D2). The copy is semantically identical, so this is a one-time warning
/// rather than an error.
pub(in crate::daemon::server) fn note_reflink_fallback(cache_file: &Path, out_path: &Path) {
    DELIVERED[3].fetch_add(1, Ordering::Relaxed);
    if !REFLINK_FALLBACK_WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            event = "materialization_reflink_fallback",
            cache_file = %cache_file.display(),
            out_path = %out_path.display(),
            "ZCCACHE_MODE=REFLINK: the volume cannot reflink here; delivering independent copies instead"
        );
    }
}

/// Service-wide default mode: an embedded host's `ZCCACHE_MODE` at start or
/// its explicit setting. A standalone daemon has none — it is spawned lazily
/// from whichever shell ran first, so its own environment says nothing about
/// later clients. `None` means "AUTO unless a request says otherwise".
#[derive(Debug)]
pub(in crate::daemon::server) struct MaterializationModeDefault(AtomicU8);

const NO_DEFAULT: u8 = u8::MAX;

impl MaterializationModeDefault {
    pub(in crate::daemon::server) fn new(mode: Option<MaterializationMode>) -> Self {
        Self(AtomicU8::new(encode(mode)))
    }

    pub(in crate::daemon::server) fn get(&self) -> Option<MaterializationMode> {
        decode(self.0.load(Ordering::Relaxed))
    }

    pub(in crate::daemon::server) fn set(&self, mode: Option<MaterializationMode>) {
        self.0.store(encode(mode), Ordering::Relaxed);
    }
}

fn encode(mode: Option<MaterializationMode>) -> u8 {
    mode.map_or(NO_DEFAULT, |mode| {
        MaterializationMode::ALL
            .iter()
            .position(|candidate| *candidate == mode)
            .map_or(NO_DEFAULT, |index| index as u8)
    })
}

fn decode(raw: u8) -> Option<MaterializationMode> {
    MaterializationMode::ALL.get(usize::from(raw)).copied()
}

/// Resolve one request's mode: a valid client `ZCCACHE_MODE` wins, then the
/// service default, then `AUTO`. An invalid client value is reported and
/// skipped — the CLI rejects it before dispatch, so reaching here means an
/// embedding host forwarded it unvalidated.
pub(in crate::daemon::server) fn resolve_request_mode(
    client_env: Option<&[(String, String)]>,
    service_default: Option<MaterializationMode>,
) -> MaterializationMode {
    match crate::core::config::materialization_mode_from_client_env(client_env) {
        Ok(Some(mode)) => mode,
        Ok(None) => service_default.unwrap_or_default(),
        Err(error) => {
            tracing::warn!(
                event = "materialization_mode_invalid",
                %error,
                "ignoring invalid request ZCCACHE_MODE"
            );
            service_default.unwrap_or_default()
        }
    }
}

impl SharedState {
    /// The materialization mode for one request (see [`resolve_request_mode`]).
    pub(in crate::daemon::server) fn materialization_mode(
        &self,
        client_env: Option<&[(String, String)]>,
    ) -> MaterializationMode {
        resolve_request_mode(client_env, self.materialization_mode_default.get())
    }
}

impl EmbeddedDaemon {
    /// Replace the service-wide mode default (see
    /// [`crate::embedded::ZccacheService::set_materialization_mode`]).
    pub(crate) fn set_materialization_mode_default(&self, mode: Option<MaterializationMode>) {
        self.state.materialization_mode_default.set(mode);
    }

    pub(crate) fn materialization_mode_default(&self) -> Option<MaterializationMode> {
        self.state.materialization_mode_default.get()
    }
}

#[cfg(test)]
#[path = "delivery_mode_tests.rs"]
mod tests;

//! Experimental `.pack` artifact format (env-gated via `ZCCACHE_PACK_ARTIFACTS`).
//!
//! Layout of `{key_hex}.pack`:
//!
//!   [magic: 4 bytes = b"ZCPK"]
//!   [num_payloads: u32 le]
//!   [(offset: u64 le, size: u64 le)] * num_payloads
//!   [payload_0 bytes]
//!   [payload_1 bytes]
//!   ...
//!
//! Why: each `std::fs::write` of a fresh file under Windows Defender pays a
//! per-file scan cost. Packing N payloads of one cache miss into a single
//! `.pack` collapses the per-file overhead by N. Bench measured 2.6× wall-clock
//! improvement at 5 payloads per artifact (see `tests/persist_pool_bench.rs`).
//!
//! Trade-off: hit path can't hardlink — it must slice the pack and write the
//! extracted bytes. Gated by `ZCCACHE_PACK_ARTIFACTS` until the read-path cost
//! is measured against the write-path win on real workloads.

use super::*;

pub(in crate::daemon::server) const PACK_MAGIC: &[u8; 4] = b"ZCPK";

pub(in crate::daemon::server) fn pack_mode_enabled() -> bool {
    std::env::var("ZCCACHE_PACK_ARTIFACTS")
        .ok()
        .is_some_and(|v| !v.is_empty() && v != "0")
}

pub(in crate::daemon::server) fn pack_path_for(artifact_dir: &Path, key_hex: &str) -> PathBuf {
    artifact_dir.join(format!("{key_hex}.pack"))
}

pub(in crate::daemon::server) fn build_pack(payloads: &[Arc<Vec<u8>>]) -> Vec<u8> {
    let n = payloads.len();
    let header_size = 4 + 4 + n * 16;
    let body_size: usize = payloads.iter().map(|p| p.len()).sum();
    let mut buf = Vec::with_capacity(header_size + body_size);
    buf.extend_from_slice(PACK_MAGIC);
    buf.extend_from_slice(&(n as u32).to_le_bytes());
    let mut offset = header_size as u64;
    for p in payloads {
        buf.extend_from_slice(&offset.to_le_bytes());
        buf.extend_from_slice(&(p.len() as u64).to_le_bytes());
        offset += p.len() as u64;
    }
    for p in payloads {
        buf.extend_from_slice(p);
    }
    buf
}

pub(in crate::daemon::server) fn parse_pack_header(data: &[u8]) -> std::io::Result<Vec<(u64, u64)>> {
    if data.len() < 8 || &data[..4] != PACK_MAGIC {
        return Err(std::io::Error::other("not a zccache pack file"));
    }
    let n = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
    let needed = 8 + n * 16;
    if data.len() < needed {
        return Err(std::io::Error::other("pack header truncated"));
    }
    let mut entries = Vec::with_capacity(n);
    for i in 0..n {
        let base = 8 + i * 16;
        let offset = u64::from_le_bytes(data[base..base + 8].try_into().unwrap());
        let size = u64::from_le_bytes(data[base + 8..base + 16].try_into().unwrap());
        entries.push((offset, size));
    }
    Ok(entries)
}

/// Try to extract the i-th payload from `{key_hex}.pack`. Returns None if the
/// pack file is missing, corrupt, or doesn't have that many payloads.
pub(in crate::daemon::server) fn try_load_packed_payload(
    artifact_dir: &Path,
    key_hex: &str,
    idx: usize,
) -> Option<Vec<u8>> {
    let pack_path = pack_path_for(artifact_dir, key_hex);
    let data = std::fs::read(&pack_path).ok()?;
    let entries = parse_pack_header(&data).ok()?;
    let &(offset, size) = entries.get(idx)?;
    let start = offset as usize;
    let end = start.checked_add(size as usize)?;
    if end > data.len() {
        return None;
    }
    Some(data[start..end].to_vec())
}

/// Persist all payloads of one artifact, either as N individual files
/// (today's layout) or as a single `.pack` file (env-gated). Wraps every
/// inner `std::fs::write` in `persist_artifact_output`'s tmp-then-rename
/// atomicity.
pub(in crate::daemon::server) fn persist_artifact_payloads(
    artifact_dir: &Path,
    key_hex: &str,
    payloads: &[Arc<Vec<u8>>],
) -> std::io::Result<()> {
    if pack_mode_enabled() {
        let pack = build_pack(payloads);
        return persist_artifact_output(&pack_path_for(artifact_dir, key_hex), &pack);
    }
    // Run inline for small N — rayon dispatch cost is comparable to the
    // syscalls themselves below the threshold (same break-even as
    // `write_payloads_par`). Empirically tuned in
    // `crates/zccache-daemon/benches/persist_payloads.rs`.
    if payloads.len() < PAR_WRITE_THRESHOLD {
        for (i, payload) in payloads.iter().enumerate() {
            let cache_path = artifact_dir.join(format!("{key_hex}_{i}"));
            persist_artifact_output(&cache_path, payload)?;
        }
        return Ok(());
    }
    use rayon::prelude::*;
    // `reduce` preserves the prior "return first error" semantics:
    // `a.and(b)` returns the first `Err` it sees and otherwise `Ok(())`.
    payloads
        .par_iter()
        .enumerate()
        .map(|(i, payload)| {
            let cache_path = artifact_dir.join(format!("{key_hex}_{i}"));
            persist_artifact_output(&cache_path, payload)
        })
        .reduce(|| Ok(()), |a, b| a.and(b))
}

//! Typed, exact benchmark records for the public stats publisher (#1754).
//!
//! Human Markdown tables remain in each test's stderr. The machine consumer
//! uses only these versioned JSON records, never display-rounded table cells.

use std::path::Path;
use std::time::Duration;

use serde::Serialize;

pub const RECORD_PREFIX: &str = "ZCCACHE_BENCH_METRIC_V1 ";

#[derive(Clone, Debug, Serialize)]
pub struct SccacheEvidence {
    pub status: &'static str,
    pub hits: Option<u64>,
    pub misses: Option<u64>,
    pub non_cacheable: Option<u64>,
    pub cache_location: Option<String>,
}

impl SccacheEvidence {
    pub fn unavailable() -> Self {
        Self {
            status: "unavailable",
            hits: None,
            misses: None,
            non_cacheable: None,
            cache_location: None,
        }
    }

    pub fn unverified() -> Self {
        Self {
            status: "unverified",
            hits: None,
            misses: None,
            non_cacheable: None,
            cache_location: None,
        }
    }

    pub fn from_stats(stats: &SccacheStats, mode: &'static str) -> Self {
        let status =
            if mode == "warm" && stats.hits > 0 && stats.misses == 0 && stats.non_cacheable == 0 {
                "hit"
            } else if stats.non_cacheable > 0 {
                "non_cacheable"
            } else if stats.misses > 0 {
                "miss"
            } else {
                "unverified"
            };
        Self {
            status,
            hits: Some(stats.hits),
            misses: Some(stats.misses),
            non_cacheable: Some(stats.non_cacheable),
            cache_location: stats.cache_location.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SccacheStats {
    pub hits: u64,
    pub misses: u64,
    pub non_cacheable: u64,
    pub cache_location: Option<String>,
}

impl SccacheStats {
    pub fn delta(&self, previous: &Self) -> Self {
        Self {
            hits: self.hits.saturating_sub(previous.hits),
            misses: self.misses.saturating_sub(previous.misses),
            non_cacheable: self.non_cacheable.saturating_sub(previous.non_cacheable),
            cache_location: self.cache_location.clone(),
        }
    }
}

/// Capture the same server's JSON counters while it is still running. A
/// failure returns `None` and the publisher will not claim a verified hit.
pub fn sccache_stats(sccache: &Path) -> Option<SccacheStats> {
    let output = std::process::Command::new(sccache)
        .args(["--show-stats", "--stats-format=json"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let stats = value.get("stats")?;
    let count = |name: &str| -> Option<u64> {
        stats
            .get(name)?
            .get("counts")?
            .as_object()?
            .values()
            .try_fold(0_u64, |sum, value| sum.checked_add(value.as_u64()?))
    };
    let not_cached = stats
        .get("non_cacheable_compilations")?
        .as_u64()?
        .checked_add(stats.get("requests_not_cacheable")?.as_u64()?)?;
    Some(SccacheStats {
        hits: count("cache_hits")?,
        misses: count("cache_misses")?,
        non_cacheable: not_cached,
        cache_location: value
            .get("cache_location")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    })
}

#[derive(Serialize)]
pub struct MetricRow<'a> {
    pub schema_version: u8,
    pub benchmark: &'a str,
    pub language: &'a str,
    pub test_name: &'a str,
    pub scenario_id: &'a str,
    pub scenario: &'a str,
    pub mode: &'a str,
    pub methodology: &'a str,
    pub trial_count: usize,
    pub bare_label: &'a str,
    pub bare_duration_ns: u128,
    pub sccache_duration_ns: Option<u128>,
    pub zccache_duration_ns: u128,
    pub bare_cache_bytes: u64,
    pub sccache_cache_bytes: Option<u64>,
    pub zccache_cache_bytes: u64,
    pub sccache_evidence: SccacheEvidence,
}

impl<'a> MetricRow<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        benchmark: &'a str,
        language: &'a str,
        test_name: &'a str,
        scenario_id: &'a str,
        scenario: &'a str,
        mode: &'a str,
        methodology: &'a str,
        trial_count: usize,
        bare_label: &'a str,
        bare: Duration,
        sccache: Option<Duration>,
        zccache: Duration,
        sccache_cache_bytes: Option<u64>,
        zccache_cache_bytes: u64,
        sccache_evidence: SccacheEvidence,
    ) -> Self {
        Self {
            schema_version: 1,
            benchmark,
            language,
            test_name,
            scenario_id,
            scenario,
            mode,
            methodology,
            trial_count,
            bare_label,
            bare_duration_ns: bare.as_nanos(),
            sccache_duration_ns: sccache.map(|duration| duration.as_nanos()),
            zccache_duration_ns: zccache.as_nanos(),
            bare_cache_bytes: 0,
            sccache_cache_bytes,
            zccache_cache_bytes,
            sccache_evidence,
        }
    }
}

pub fn emit_metric(row: &MetricRow<'_>) {
    let json = serde_json::to_string(row).expect("metric record must serialize");
    eprintln!("{RECORD_PREFIX}{json}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_record_preserves_nanoseconds() {
        let row = MetricRow::new(
            "rust",
            "rust",
            "perf_test",
            "build",
            "Build, Warm",
            "warm",
            "rustc-batch",
            5,
            "Bare rustc",
            Duration::from_nanos(360_000_000),
            Some(Duration::from_nanos(360_000_000)),
            Duration::from_nanos(878_000),
            Some(1024),
            2048,
            SccacheEvidence::unverified(),
        );
        let value = serde_json::to_value(row).unwrap();
        assert_eq!(value["zccache_duration_ns"], 878_000);
        assert_eq!(value["sccache_evidence"]["status"], "unverified");
    }
}

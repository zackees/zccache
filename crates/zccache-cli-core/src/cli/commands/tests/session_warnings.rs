use crate::cli::commands::session::session_summary_warnings;
use crate::protocol::{LookupOutcomes, SessionStats};

fn stats_with(outcomes: LookupOutcomes) -> SessionStats {
    SessionStats {
        duration_ms: 1000,
        compilations: 10,
        hits: 5,
        misses: 5,
        non_cacheable: 0,
        errors: 0,
        errors_cached: 0,
        time_saved_ms: 0,
        unique_sources: 5,
        bytes_read: 0,
        bytes_written: 0,
        lookup_outcomes: outcomes,
        phase_profile: None,
    }
}

#[test]
fn summary_warning_fires_for_wasted_depgraph_hits_over_threshold() {
    let warnings = session_summary_warnings(
        &stats_with(LookupOutcomes {
            depgraph_hit_artifact_hit: 80,
            depgraph_hit_artifact_miss: 12,
            depgraph_cold_skip: 8,
            depgraph_other_miss: Default::default(),
        }),
        true,
        false,
    );

    assert_eq!(warnings.len(), 1);
    let rendered = warnings[0].render(false);
    assert!(rendered.contains("12.0% of depgraph hits"));
    assert!(rendered.contains("#680 / #796 SI-3"));
}

#[test]
fn summary_warning_fires_for_cold_skip_dominating_with_depgraph_activity() {
    let warnings = session_summary_warnings(
        &stats_with(LookupOutcomes {
            depgraph_hit_artifact_hit: 10,
            depgraph_hit_artifact_miss: 0,
            depgraph_cold_skip: 90,
            depgraph_other_miss: Default::default(),
        }),
        true,
        false,
    );

    assert_eq!(warnings.len(), 1);
    let rendered = warnings[0].render(false);
    assert!(rendered.contains("90.0% of lookups returned cold_skip"));
    assert!(rendered.contains("#320 / #796 SI-2"));
}

#[test]
fn summary_warning_does_not_fire_for_cold_skip_when_depgraph_missing() {
    let warnings = session_summary_warnings(
        &stats_with(LookupOutcomes {
            depgraph_hit_artifact_hit: 0,
            depgraph_hit_artifact_miss: 0,
            depgraph_cold_skip: 100,
            depgraph_other_miss: Default::default(),
        }),
        false,
        false,
    );

    assert!(warnings.is_empty());
}

#[test]
fn summary_warning_verbose_controls_catastrophic_hit_rate() {
    let mut stats = stats_with(LookupOutcomes {
        depgraph_hit_artifact_hit: 1,
        depgraph_hit_artifact_miss: 0,
        depgraph_cold_skip: 0,
        depgraph_other_miss: Default::default(),
    });
    stats.compilations = 80;
    stats.hits = 1;
    stats.misses = 79;

    assert!(session_summary_warnings(&stats, true, false).is_empty());
    let warnings = session_summary_warnings(&stats, true, true);
    assert_eq!(warnings.len(), 1);
    let rendered = warnings[0].render(false);
    assert!(rendered.contains("catastrophic hit rate (1.2%)"));
    assert!(rendered.contains("80 compilations"));
}

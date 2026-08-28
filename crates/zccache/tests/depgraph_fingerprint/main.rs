//! Dependency-graph and fingerprint integration tests (#1526).
//!
//! Covers depgraph persistence, warm classification, depfile ingestion
//! and drift detection, the fingerprint matrices, and lineage
//! propagation. Each former top-level `tests/*.rs` file is a module here,
//! so the category links one test executable instead of 15. Test IDs are
//! `depgraph_fingerprint::<module>::<test_name>`.
//!
//! Per-module lint allows and `#![cfg(...)]` gates stay as the inner
//! attributes at the top of each file, so nothing is widened to a
//! common denominator.

#[path = "../common/mod.rs"]
mod common;

mod daemon_depgraph_persistence_test;
mod daemon_depgraph_warm_classify_test;
mod daemon_fingerprint_test;
mod daemon_lineage_propagation;
mod depgraph_depfile_integration_test;
mod depgraph_drift_detection_test;
mod depgraph_stress_adversarial_test;
mod depgraph_stress_concurrent_test;
mod depgraph_stress_integration_test;
mod fingerprint_concurrent;
mod fingerprint_edge_cases;
mod fingerprint_end_to_end;
mod fingerprint_glob_scan;
mod fingerprint_stress;
mod symbols_stamp_roundtrip;

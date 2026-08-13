//! Neutral executable mechanics: native executable/script/library suffixes,
//! PATH/PATHEXT lookup, running-image discovery and equality, and
//! runnable-image materialization/replacement.
//!
//! Version-rooted deployment policy, GC, and daemon lifecycle decisions
//! stay with the CLI. Populated in the executable phase (#1370); empty index
//! until then.

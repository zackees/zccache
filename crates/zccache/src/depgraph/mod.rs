//! Dependency graph for include-aware cache invalidation.
//!
//! Tracks `#include` relationships between source and header files,
//! resolves include paths against search directories, and determines
//! whether a compilation can use a cached artifact.

pub mod args;
pub mod compile_commands;
pub mod context;
pub mod depfile;
pub mod graph;
pub mod msvc_args;
pub mod rustc_args;
pub mod scanner;
pub mod search_paths;
pub mod session;
pub mod show_includes;
pub mod snapshot;
pub mod system_includes;
pub mod watcher_support;

pub use args::{ParsedArgs, UserDepFlags};
pub use compile_commands::{parse_compile_commands_json, CompileCommand};
pub use context::{
    compute_artifact_key, compute_rustc_artifact_key, ArtifactKey, CompileContext, ContextKey,
    RustcCompileContext,
};
pub use depfile::{prepare_depfile, DepfileError, DepfileStrategy};
pub use graph::{CacheVerdict, ContextState, DepGraph, DepGraphStats};
pub use rustc_args::{parse_rustc_args, ExternCrate, RustcParsedArgs};
pub use scanner::{IncludeDirective, IncludeKind, ScanResult};
pub use search_paths::IncludeSearchPaths;
pub use session::{
    FinalizedSessionStats, Session, SessionConfig, SessionId, SessionManager, SessionStatsTracker,
};
pub use snapshot::{
    classify_load, depgraph_file_path, load_from_file, save_to_file, DepGraphLoadOutcome,
    SnapshotError, DEPGRAPH_VERSION,
};
pub use system_includes::{
    discovery_args, discovery_args_fast, parse_cc1_system_include_output,
    parse_system_include_output, SystemIncludeCache,
};
pub use watcher_support::WatchSet;

//! Shared result types for multi-source cache checks and staged execution.

use super::*;

pub(in crate::daemon::server) struct PendingWrite {
    pub(in crate::daemon::server) out_path: NormalizedPath,
    pub(in crate::daemon::server) cache_file: NormalizedPath,
    pub(in crate::daemon::server) data: Vec<u8>,
}

pub(in crate::daemon::server) enum UnitCacheResult {
    Hit {
        stdout: Arc<Vec<u8>>,
        stderr: Arc<Vec<u8>>,
        artifact_bytes: u64,
        source_path: NormalizedPath,
        pending_writes: Vec<PendingWrite>,
    },
    Miss {
        source_path: NormalizedPath,
        output_path: NormalizedPath,
        context_key: ContextKey,
        ctx: Box<CompileContext>,
        input_snapshot: InputSnapshot,
    },
}

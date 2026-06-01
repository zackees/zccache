//! Compile request pipeline facade.
//!
//! The hot compile path is split into focused submodules so soldr-facing
//! changes can target request preparation, cached-hit materialization, or miss
//! execution without editing one giant handler.

mod cached_hit;
mod error_cache;
mod hit_branches;
mod miss_profile;
mod miss_store;
mod pipeline;
mod request;

use super::*;
use request::CompileRequest;

// Re-export the link miss profile so `handle_link` can emit phase data
// without owning a copy of the struct. Issue #535 — the ephemeral link
// path needs the same per-phase counters the compile path already
// emits, gated on the same `ZCCACHE_PROFILE_CC_MISS` env.
pub(super) use miss_profile::{emit_link_miss_profile, LinkMissProfile};

pub(super) async fn handle_compile(
    state_arc: &Arc<SharedState>,
    session_id: &str,
    args: &[String],
    cwd: &Path,
    compiler_path: &Path,
    client_env: Option<Vec<(String, String)>>,
    stdin: Vec<u8>,
) -> Response {
    pipeline::handle_compile_request(CompileRequest {
        state_arc,
        session_id,
        args,
        cwd,
        compiler_path,
        client_env,
        stdin,
    })
    .await
}

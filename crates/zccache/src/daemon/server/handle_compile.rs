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

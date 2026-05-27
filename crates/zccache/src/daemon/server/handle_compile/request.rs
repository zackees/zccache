//! Request object for the compile pipeline.

use super::super::*;

pub(super) struct CompileRequest<'a> {
    pub(super) state_arc: &'a Arc<SharedState>,
    pub(super) session_id: &'a str,
    pub(super) args: &'a [String],
    pub(super) cwd: &'a Path,
    pub(super) compiler_path: &'a Path,
    pub(super) client_env: Option<Vec<(String, String)>>,
    pub(super) stdin: Vec<u8>,
}

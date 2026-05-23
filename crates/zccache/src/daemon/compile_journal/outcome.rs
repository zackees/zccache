//! Map a [`zccache::protocol::Response`] to a journal-ready tuple.

use zccache::protocol::Response;

use super::miss_reason;

/// Extract outcome string, exit code, and default miss reason from a Response.
///
/// Returns `None` for non-compile/link responses (Ping, Status, etc.). The
/// third tuple element is `Some(_)` only when the outcome is `"miss"` or
/// `"link_miss"` (issue #322 acceptance criteria #1: every miss carries a
/// reason). Concrete attributions (`context_not_found`, etc.) are layered
/// on by the compile-handler in follow-up work; this function is the
/// canonical *default* so the journal-writer cannot accidentally omit the
/// field.
pub fn extract_outcome(response: &Response) -> Option<(&'static str, i32, Option<&'static str>)> {
    match response {
        Response::CompileResult {
            exit_code, cached, ..
        } => {
            if *exit_code != 0 {
                Some(("error", *exit_code, None))
            } else if *cached {
                Some(("hit", *exit_code, None))
            } else {
                Some(("miss", *exit_code, Some(miss_reason::UNKNOWN)))
            }
        }
        Response::LinkResult {
            exit_code, cached, ..
        } => {
            if *exit_code != 0 {
                Some(("error", *exit_code, None))
            } else if *cached {
                Some(("link_hit", *exit_code, None))
            } else {
                Some(("link_miss", *exit_code, Some(miss_reason::UNKNOWN)))
            }
        }
        Response::Error { .. } => Some(("error", -1, None)),
        _ => None,
    }
}

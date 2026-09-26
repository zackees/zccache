//! `zccache status` — human-readable and JSON daemon status.

use std::process::ExitCode;

use super::util::{
    format_bytes, format_duration_ms, format_uptime, print_json_value, LOST_CONNECTION_MSG,
};

pub(crate) async fn cmd_status(endpoint: &str, json: bool) -> ExitCode {
    let recv_result = match crate::ipc::daemon_control_roundtrip(
        endpoint,
        crate::ipc::DaemonControlRequest::Status,
        None,
    )
    .await
    {
        Ok(response) => response,
        Err(e) if crate::cli::client::is_daemon_unreachable_err(&e) => {
            let message = format!("daemon not running at {endpoint}: {e}");
            if json {
                print_status_error_json(endpoint, &message);
            } else {
                eprintln!("{message}");
            }
            return ExitCode::FAILURE;
        }
        Err(e) => {
            let message = format!("zccache: broken connection to daemon: {e}");
            if json {
                print_status_error_json(endpoint, &message);
            } else {
                eprintln!("{message}");
            }
            return ExitCode::FAILURE;
        }
    };
    match recv_result {
        Some(crate::protocol::Response::Status(s)) => {
            if json {
                print_status_ok_json(endpoint, &s);
                return ExitCode::SUCCESS;
            }
            let total = s.cache_hits + s.cache_misses;
            let hit_rate = if total > 0 {
                format!("{:.1}%", s.cache_hits as f64 / total as f64 * 100.0)
            } else {
                "n/a".to_string()
            };

            println!(
                "zccache daemon v{} (protocol v{}) ({}) — uptime {}",
                if s.version.is_empty() {
                    "unknown"
                } else {
                    &s.version
                },
                crate::protocol::PROTOCOL_VERSION,
                endpoint,
                format_uptime(s.uptime_secs)
            );
            if !s.cache_dir.as_os_str().is_empty() {
                println!("cache dir: {}", s.cache_dir.display());
            }
            println!("namespace: {}", s.daemon_namespace);
            if s.private_daemon.enabled {
                let owners = if s.private_daemon.owners.is_empty() {
                    "none".to_string()
                } else {
                    s.private_daemon
                        .owners
                        .iter()
                        .map(|owner| format!("{}x{}", owner.pid, owner.ref_count))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let env_keys = if s.private_daemon.private_env_keys.is_empty() {
                    "none".to_string()
                } else {
                    s.private_daemon.private_env_keys.join(", ")
                };
                println!("private daemon: yes");
                println!("private owners: {owners}");
                println!("private env keys: {env_keys}");
            }
            println!();
            println!(
                "  Compilations:  {} total ({} cached, {} cold, {} non-cacheable)",
                s.total_compilations, s.cache_hits, s.cache_misses, s.non_cacheable
            );
            println!("  Hit rate:      {hit_rate}");
            if s.time_saved_ms > 0 {
                println!("  Time saved:    ~{}", format_duration_ms(s.time_saved_ms));
            }
            if s.compile_errors > 0 {
                println!("  Errors:        {}", s.compile_errors);
            }
            if s.compile_errors_cached > 0 {
                println!("  Cached errors: {}", s.compile_errors_cached);
            }
            println!();
            println!(
                "  Artifacts:     {} ({})",
                s.artifact_count,
                format_bytes(s.cache_size_bytes)
            );
            {
                let disk_info = if s.dep_graph_disk_size > 0 {
                    format!(
                        "v{}, persisted, {} on disk",
                        s.dep_graph_version,
                        format_bytes(s.dep_graph_disk_size)
                    )
                } else if s.dep_graph_persisted {
                    // Save has flushed at least once, but the file metadata
                    // call lost a race (e.g. rename window) — still persisted.
                    format!("v{}, persisted", s.dep_graph_version)
                } else {
                    format!("v{}, not persisted", s.dep_graph_version)
                };
                println!(
                    "  Dep graph:     {} contexts, {} files ({})",
                    s.dep_graph_contexts, s.dep_graph_files, disk_info
                );
            }
            println!("  Metadata:      {} entries", s.metadata_entries);
            {
                // Watcher state is a performance cliff, not a cosmetic detail:
                // with no watcher both fast hit tiers are disabled and every
                // shared cache blob is re-hashed on read (issue #1156).
                let watcher = match (s.watcher_active, s.watcher_degradations) {
                    (true, 0) => "active".to_string(),
                    (true, n) => format!("active (recovered from {n} degradation(s))"),
                    (false, n) => {
                        format!("DEGRADED — fast hit tiers disabled ({n} failure(s), retrying)")
                    }
                };
                println!("  Watcher:       {watcher}");
            }
            println!(
                "  Delivery:      {}",
                format_materialization(&s.materialization)
            );
            if s.index_writer_gone {
                // Worse than a performance cliff: the daemon still serves from
                // memory, so it looks healthy, but nothing it publishes is
                // being recorded and none of it survives a restart (#1177).
                println!(
                    "  Index writer:  GONE — new artifacts are not being recorded and will \
                     not survive a restart; restart the daemon"
                );
            }
            println!();
            if s.total_links > 0 {
                println!();
                let link_total = s.link_hits + s.link_misses;
                let link_hit_rate = if link_total > 0 {
                    format!("{:.1}%", s.link_hits as f64 / link_total as f64 * 100.0)
                } else {
                    "n/a".to_string()
                };
                println!(
                    "  Links:         {} total ({} cached, {} cold, {} non-cacheable)",
                    s.total_links, s.link_hits, s.link_misses, s.link_non_cacheable
                );
                println!("  Link hit rate: {link_hit_rate}");
            }
            println!();
            println!(
                "  Sessions:      {} active / {} total",
                s.sessions_active, s.sessions_total
            );
            ExitCode::SUCCESS
        }
        None => {
            let message = LOST_CONNECTION_MSG;
            if json {
                print_status_error_json(endpoint, message);
            } else {
                eprintln!("{message}");
            }
            ExitCode::FAILURE
        }
        Some(other) => {
            let message = format!("zccache: unexpected response from daemon: {other:?}");
            if json {
                print_status_error_json(endpoint, &message);
            } else {
                eprintln!("{message}");
            }
            ExitCode::FAILURE
        }
    }
}

/// One-line `ZCCACHE_MODE` delivery summary (#1683): the service-default
/// mode and per-tier counts since the daemon started, with REFLINK fallbacks
/// called out because they mean the volume could not clone.
fn format_materialization(status: &crate::protocol::MaterializationStatus) -> String {
    let mode = if status.mode.is_empty() {
        "AUTO"
    } else {
        status.mode.as_str()
    };
    let mut line = format!(
        "{mode} ({} reflink, {} hardlink, {} copy)",
        status.reflink, status.hardlink, status.copy
    );
    if status.reflink_fallbacks > 0 {
        line.push_str(&format!(
            " — {} REFLINK fallback(s) to copy",
            status.reflink_fallbacks
        ));
    }
    line
}

fn print_status_ok_json(endpoint: &str, s: &crate::protocol::DaemonStatus) {
    let total = s.cache_hits + s.cache_misses;
    let hit_rate = if total > 0 {
        Some(s.cache_hits as f64 / total as f64)
    } else {
        None
    };
    let link_total = s.link_hits + s.link_misses;
    let link_hit_rate = if link_total > 0 {
        Some(s.link_hits as f64 / link_total as f64)
    } else {
        None
    };
    let value = serde_json::json!({
        "status": "ok",
        "endpoint": endpoint,
        "daemon_namespace": s.daemon_namespace,
        "private_daemon": s.private_daemon,
        "protocol_version": crate::protocol::PROTOCOL_VERSION,
        "hit_rate": hit_rate,
        "link_hit_rate": link_hit_rate,
        "daemon": s,
    });
    print_json_value(&value);
}

fn print_status_error_json(endpoint: &str, message: &str) {
    let value = serde_json::json!({
        "status": "error",
        "endpoint": endpoint,
        "daemon_namespace": crate::core::config::daemon_namespace_label(),
        "error": message,
    });
    print_json_value(&value);
}

#[cfg(test)]
mod materialization_tests {
    use super::format_materialization;
    use crate::protocol::MaterializationStatus;

    #[test]
    fn delivery_line_shows_mode_and_tier_counts() {
        let status = MaterializationStatus {
            mode: "COPY".to_string(),
            reflink: 0,
            hardlink: 2,
            copy: 40,
            reflink_fallbacks: 0,
        };
        assert_eq!(
            format_materialization(&status),
            "COPY (0 reflink, 2 hardlink, 40 copy)"
        );
    }

    #[test]
    fn delivery_line_calls_out_reflink_fallbacks_and_defaults_to_auto() {
        let status = MaterializationStatus {
            mode: String::new(),
            reflink: 1,
            hardlink: 0,
            copy: 3,
            reflink_fallbacks: 3,
        };
        assert_eq!(
            format_materialization(&status),
            "AUTO (1 reflink, 0 hardlink, 3 copy) — 3 REFLINK fallback(s) to copy"
        );
    }
}

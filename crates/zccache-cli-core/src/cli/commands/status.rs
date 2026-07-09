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

//! Compiler-termination projection for embedded audit events.

use crate::daemon::server::EmbeddedCompileResult;

pub(super) fn emit_compile_finished(
    service: &super::ZccacheService,
    audit: &crate::audit::AuditContext,
    response: &EmbeddedCompileResult,
    duration_ns: u64,
) {
    if service.audit_sink.is_none() && service.event_sink.is_none() {
        return;
    }
    let base_fields = [
        ("exit_code", serde_json::Value::from(response.exit_code)),
        ("cached", serde_json::Value::from(response.cached)),
    ];
    let level = if response.exit_code == 0 {
        crate::audit::AuditLevel::Info
    } else {
        crate::audit::AuditLevel::Error
    };
    if let Some(signal) =
        crate::platform::process::exit::termination_signal_from_exit_code(response.exit_code)
    {
        let fields = [
            ("exit_code", serde_json::Value::from(response.exit_code)),
            ("cached", serde_json::Value::from(response.cached)),
            ("termination_signal", serde_json::Value::from(signal)),
        ];
        service.emit_audit(
            audit,
            crate::audit::AuditCategory::ZCCACHE_COMPILE,
            crate::audit::AuditEventName::COMPILE_FINISHED,
            level,
            Some(duration_ns),
            &fields,
        );
    } else {
        service.emit_audit(
            audit,
            crate::audit::AuditCategory::ZCCACHE_COMPILE,
            crate::audit::AuditEventName::COMPILE_FINISHED,
            level,
            Some(duration_ns),
            &base_fields,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn response(exit_code: i32) -> EmbeddedCompileResult {
        EmbeddedCompileResult {
            exit_code,
            stdout: Arc::new(Vec::new()),
            stderr: Arc::new(Vec::new()),
            cached: false,
        }
    }

    #[test]
    fn response_encoding_recovers_only_exact_signals() {
        assert_eq!(response(-143).exit_code, -143);
        #[cfg(unix)]
        assert_eq!(
            crate::platform::process::exit::termination_signal_from_exit_code(-143),
            Some(15)
        );
        #[cfg(windows)]
        assert_eq!(
            crate::platform::process::exit::termination_signal_from_exit_code(-143),
            None
        );
        for exit_code in [0, 1, -1] {
            assert_eq!(
                crate::platform::process::exit::termination_signal_from_exit_code(exit_code),
                None
            );
        }
    }

    #[cfg(unix)]
    fn find_named_file(dir: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = find_named_file(&path, name) {
                    return Some(found);
                }
            } else if path.file_name().and_then(|value| value.to_str()) == Some(name) {
                return Some(path);
            }
        }
        None
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cacheable_rust_compile_preserves_signal_in_response_journal_and_audit() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::TempDir::new().expect("temp root");
        let compiler = temp.path().join("rustc");
        let count = temp.path().join("compile-count");
        let source = temp.path().join("signal.rs");
        let output = temp.path().join("libsignal.rmeta");
        std::fs::write(&source, "pub fn signal_fixture() {}\n").expect("source");
        std::fs::write(
            &compiler,
            "#!/bin/sh\nif [ \"$1\" = \"-vV\" ]; then\n  printf 'rustc 1.95.0 (fixture 2026-01-01)\\nbinary: rustc\\ncommit-hash: fixture\\ncommit-date: 2026-01-01\\nhost: x86_64-unknown-linux-gnu\\nrelease: 1.95.0\\nLLVM version: 20.1.0\\n'\n  exit 0\nfi\nprintf x >> \"$ZCCACHE_SIGNAL_COUNT\"\nkill -TERM $$\n",
        )
        .expect("compiler script");
        std::fs::set_permissions(&compiler, std::fs::Permissions::from_mode(0o755))
            .expect("make compiler executable");

        let service = super::super::ZccacheService::start(super::super::ZccacheConfig {
            host: super::super::HostIdentity {
                product: "signal-test".into(),
                instance_id: "cacheable-rust-signal".into(),
                workspace_id: "cacheable-rust-signal".into(),
            },
            cache_root: temp.path().join("cache").into(),
            audit: super::super::AuditConfig {
                output_root: Some(temp.path().join("audit").to_string_lossy().into_owned()),
                ..super::super::AuditConfig::default()
            },
            limits: super::super::ServiceLimits::default(),
            runtime: super::super::RuntimeHooks::default(),
            cancellation: None,
        })
        .await
        .expect("service start");
        let request = super::super::CompileRequest {
            audit: super::super::AuditContext::new(
                crate::audit::AuditId::new("signal-run").expect("run id"),
                crate::audit::AuditId::new("signal-trace").expect("trace id"),
            ),
            compiler: compiler.into(),
            args: vec![
                source.to_string_lossy().into_owned(),
                "--crate-name".into(),
                "signal_fixture".into(),
                "--crate-type".into(),
                "lib".into(),
                "--emit=dep-info,metadata".into(),
                "-o".into(),
                output.to_string_lossy().into_owned(),
            ],
            cwd: temp.path().into(),
            env: vec![(
                "ZCCACHE_SIGNAL_COUNT".into(),
                count.to_string_lossy().into_owned(),
            )],
            stdin: Vec::new(),
        };

        for _ in 0..2 {
            let response = service
                .compile(request.clone())
                .await
                .expect("signal is a compiler response");
            assert_eq!(response.exit_code, -143);
            assert!(!response.cached);
        }
        let stats = service.stats().await.expect("stats");
        assert_eq!(stats.non_cacheable, 0, "request must use compile_exec");
        assert_eq!(std::fs::read(&count).expect("compile count"), b"xx");
        service.flush().await.expect("flush audit");

        let audit_path = find_named_file(temp.path(), "audit.jsonl").expect("audit log");
        let audit = std::fs::read_to_string(audit_path).expect("read audit");
        assert!(audit.lines().any(|line| {
            let value: serde_json::Value = serde_json::from_str(line).expect("audit JSON");
            value["event"] == "compile.finished" && value["fields"]["termination_signal"] == 15
        }));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let found = find_named_file(temp.path(), "compile_journal.jsonl")
                .and_then(|path| std::fs::read_to_string(path).ok())
                .is_some_and(|journal| {
                    journal.lines().any(|line| {
                        let value: serde_json::Value =
                            serde_json::from_str(line).expect("journal JSON");
                        value["exit_code"] == -143 && value["termination_signal"] == 15
                    })
                });
            if found {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "signal journal row");
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        service
            .shutdown(super::super::ShutdownMode::Graceful)
            .await
            .expect("shutdown");
    }
}

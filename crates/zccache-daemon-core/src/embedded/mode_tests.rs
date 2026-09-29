//! Embedded `ZCCACHE_MODE` setting (#1683): a host can set the service-wide
//! default, and a request's forwarded `ZCCACHE_MODE` still wins.

use super::super::*;
use crate::core::config::MaterializationMode;
use tempfile::TempDir;

async fn start_service(temp: &TempDir) -> ZccacheService {
    let mut audit = AuditConfig::default();
    audit.mode = crate::audit::AuditMode::Off;
    ZccacheService::start(ZccacheConfig {
        host: HostIdentity {
            product: "mode-test".into(),
            instance_id: "mode-instance".into(),
            workspace_id: "mode-workspace".into(),
        },
        cache_root: temp.path().join("cache").into(),
        audit,
        limits: ServiceLimits::default(),
        runtime: RuntimeHooks::default(),
        cancellation: None,
    })
    .await
    .expect("service start")
}

/// A `--crate-type=rlib` compile: its `.rlib` is hardlink-eligible, so LINK
/// and COPY deliver it differently.
fn rlib_request(
    compiler: crate::core::NormalizedPath,
    temp: &TempDir,
    env: &[(&str, &str)],
) -> CompileRequest {
    CompileRequest {
        audit: AuditContext::new(
            crate::audit::AuditId::new("mode-run").expect("id"),
            crate::audit::AuditId::new("mode-trace").expect("id"),
        ),
        compiler,
        args: vec![
            "--crate-name".into(),
            "modecrate".into(),
            "--crate-type=rlib".into(),
            "--emit=metadata,link".into(),
            "--out-dir".into(),
            temp.path().join("out").to_string_lossy().into_owned(),
            temp.path()
                .join("modecrate.rs")
                .to_string_lossy()
                .into_owned(),
        ],
        cwd: temp.path().into(),
        env: env
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
        stdin: Vec::new(),
    }
}

#[tokio::test]
async fn the_service_default_is_settable_and_clearable() {
    let temp = TempDir::new().expect("tempdir");
    let service = start_service(&temp).await;
    for mode in MaterializationMode::ALL {
        service.set_materialization_mode(Some(mode));
        assert_eq!(service.materialization_mode(), Some(mode));
    }
    service.set_materialization_mode(None);
    assert_eq!(service.materialization_mode(), None);
    service
        .shutdown(ShutdownMode::Graceful)
        .await
        .expect("shutdown");
}

fn volume_supports_hardlinks(temp: &TempDir) -> bool {
    let probe = temp.path().join("hardlink-probe");
    std::fs::write(&probe, b"x").expect("probe");
    let linked = std::fs::hard_link(&probe, temp.path().join("hardlink-probe-2")).is_ok();
    let _ = std::fs::remove_file(temp.path().join("hardlink-probe-2"));
    let _ = std::fs::remove_file(probe);
    linked
}

fn volume_supports_reflink(temp: &TempDir) -> bool {
    let probe = temp.path().join("reflink-probe");
    std::fs::write(&probe, b"x").expect("probe");
    let clone = temp.path().join("reflink-probe-2");
    let cloned = kernal_api::platform::fs::reflink_file(&probe, &clone).is_ok();
    let _ = std::fs::remove_file(clone);
    let _ = std::fs::remove_file(probe);
    cloned
}

/// (service default, request env, whether the hit must share the cache inode
/// on a volume that can hardlink but not reflink; a reflink volume never
/// shares, #1792)
type ModeCase = (
    MaterializationMode,
    &'static [(&'static str, &'static str)],
    bool,
);

/// The service default decides a hit's delivery unless the request carries
/// its own `ZCCACHE_MODE`. LINK is the positive control: it shares the
/// cache inode, so an independent COPY result is attributable to the mode.
#[tokio::test]
async fn a_hit_is_delivered_by_the_resolved_mode() {
    let Some(compiler) = crate::test_support::find_rustc() else {
        eprintln!("SKIP a_hit_is_delivered_by_the_resolved_mode: rustc not found");
        return;
    };
    let cases: [ModeCase; 6] = [
        (MaterializationMode::Link, &[], true),
        (MaterializationMode::Copy, &[], false),
        // #1792: AUTO never shares; the full chain links where it cannot clone.
        (MaterializationMode::Auto, &[], false),
        (MaterializationMode::ReflinkOrLinkOrCopy, &[], true),
        (
            MaterializationMode::ReflinkOrLinkOrCopy,
            &[("ZCCACHE_MODE", "auto")],
            false,
        ),
        (
            MaterializationMode::Link,
            &[("ZCCACHE_MODE", "copy")],
            false,
        ),
    ];
    for (service_default, request_env, expect_shared) in cases {
        let temp = TempDir::new().expect("tempdir");
        if !volume_supports_hardlinks(&temp) {
            eprintln!("SKIP a_hit_is_delivered_by_the_resolved_mode: no hardlinks here");
            return;
        }
        std::fs::write(
            temp.path().join("modecrate.rs"),
            "pub fn v() -> u32 { 5 }\n",
        )
        .expect("source");
        std::fs::create_dir_all(temp.path().join("out")).expect("out dir");
        let rlib = temp.path().join("out").join("libmodecrate.rlib");
        let label = format!("default {service_default}, request {request_env:?}");
        // LINK never clones; every other shared case clones first where it can.
        let expect_shared = expect_shared
            && (service_default == MaterializationMode::Link
                || request_env.is_empty() && !volume_supports_reflink(&temp));

        let cold = start_service(&temp).await;
        cold.set_materialization_mode(Some(service_default));
        let miss = cold
            .compile(rlib_request(compiler.clone(), &temp, request_env))
            .await
            .expect("miss compile");
        assert_eq!(miss.exit_code, 0, "{label}: {miss:?}");
        assert!(!miss.cached, "{label}");
        // Persist the artifact so the hit reads the durable cache store.
        cold.shutdown(ShutdownMode::Graceful)
            .await
            .expect("shutdown cold");
        std::fs::remove_file(&rlib).expect("remove miss rlib");

        let warm = start_service(&temp).await;
        warm.set_materialization_mode(Some(service_default));
        let hit = warm
            .compile(rlib_request(compiler.clone(), &temp, request_env))
            .await
            .expect("hit compile");
        assert!(hit.cached, "{label}: second compile must hit");
        let links = crate::platform::fs::links::hard_link_count(&rlib).expect("link count");
        if expect_shared {
            assert!(links >= 2, "{label}: the hit must share the cache inode");
        } else {
            assert_eq!(
                links, 1,
                "{label}: the hit must deliver an independent rlib"
            );
            assert!(!std::fs::metadata(&rlib)
                .expect("rlib metadata")
                .permissions()
                .readonly());
        }
        warm.shutdown(ShutdownMode::Graceful)
            .await
            .expect("shutdown warm");
    }
}

/// #1797: a request env carrying an invalid `ZCCACHE_MODE` is an error
/// response, never a silent fall back to the service default.
#[tokio::test]
async fn an_invalid_request_mode_is_an_error_response() {
    let temp = TempDir::new().expect("tempdir");
    let service = start_service(&temp).await;
    service.set_materialization_mode(Some(MaterializationMode::Copy));
    let compiler = crate::core::NormalizedPath::from(std::path::PathBuf::from("rustc"));
    let error = service
        .compile(rlib_request(compiler, &temp, &[("ZCCACHE_MODE", "blah")]))
        .await
        .expect_err("invalid mode must be an error");
    let message = format!("{error:?}");
    assert!(message.contains("ZCCACHE_MODE"), "{message}");
    assert!(message.contains("blah"), "{message}");
    assert!(message.contains("REFLINK_OR_LINK_OR_COPY"), "{message}");
    service
        .shutdown(ShutdownMode::Graceful)
        .await
        .expect("shutdown");
}

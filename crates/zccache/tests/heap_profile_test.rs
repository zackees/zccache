//! Public embedded heap-profiling contract (issue #1359).
//!
//! An integration test is a separate final executable, so its allocator
//! declaration also guards the documented downstream linkage pattern.

#![cfg(feature = "heap-profile")]

use zccache::heap_profile::{prof, MiMalloc};

#[global_allocator]
static ALLOCATOR: MiMalloc = MiMalloc;

struct ProfilerGuard;

impl Drop for ProfilerGuard {
    fn drop(&mut self) {
        prof::stop();
    }
}

#[test]
fn embedded_host_can_capture_a_pprof_heap_snapshot() {
    if prof::is_enabled() {
        prof::stop();
    }
    assert!(prof::start(1), "heap profiler should start exactly once");
    let _guard = ProfilerGuard;

    let retained = vec![0x5a_u8; 1024 * 1024];
    std::hint::black_box(&retained);

    let stats = prof::stats();
    assert!(
        stats.live_samples > 0,
        "retained allocation was not sampled"
    );
    let snapshot = prof::dump_proto_to_vec();
    assert!(
        !snapshot.is_empty(),
        "pprof profile.proto snapshot must contain the sampled allocation"
    );
}

//! Microbenchmark for the daemon persist path.
//!
//! Mirrors the structure of `handle_compile` / `handle_compile_multi`'s persist task:
//!   for each cache miss:
//!     tokio::spawn(async {
//!         let _permit = persist_semaphore.acquire().await;
//!         spawn_blocking(|| {
//!             for payload in payloads { std::fs::write(path, payload) }
//!             artifact_store.insert(key, meta)   // redb
//!         }).await
//!     })
//!
//! On Windows with Defender real-time scanning enabled, each std::fs::write
//! returns slow because Defender scans the just-written file inline. The
//! per-task serial loop + the hardcoded 8-permit semaphore cap how much
//! work can be in flight, which leaves Defender's parallel scanner idle.
//!
//! This bench isolates the worker-pool design from rustc and from the rest
//! of the daemon. It synthesizes a realistic workload (191 files, ~327 MB
//! total, matching the soldr-workspace numbers in issue #274) and runs it
//! through several persist strategies on the same machine, same disk.
//!
//! Run with:
//!   soldr cargo test -p zccache-daemon --test persist_pool_bench -- --nocapture --ignored
//!
//! Tune the workload via env vars:
//!   PERSIST_BENCH_FILES        (default 191)
//!   PERSIST_BENCH_TOTAL_MB     (default 327)
//!   PERSIST_BENCH_TRIALS       (default 1)
//!   PERSIST_BENCH_JSON         (set to write results JSON to that path)

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use tempfile::TempDir;
use zccache_monocrate::artifact::{ArtifactIndex, ArtifactStore};

/// One synthetic "compile output": a cache key + one or more payload blobs.
/// Mirrors `PersistJob` in server.rs.
struct Job {
    key_hex: String,
    payloads: Vec<Arc<Vec<u8>>>,
    meta: ArtifactIndex,
}

/// Default payloads-per-job. rustc typically emits 2-3 files per crate
/// (rlib + rmeta + .d), and that's what stresses Defender's per-file scan
/// cost. Override via `PERSIST_BENCH_PAYLOADS_PER_JOB`.
fn payloads_per_job() -> usize {
    std::env::var("PERSIST_BENCH_PAYLOADS_PER_JOB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3)
        .max(1)
}

fn make_workload(num_files: usize, total_bytes: usize, seed: u64) -> Vec<Job> {
    // Log-normal-ish size distribution: most files medium, a few large, a few tiny.
    // Matches rustc output mix (small .rmeta, large .rlib, occasional .o).
    let avg = total_bytes / num_files.max(1);
    let mut rng_state: u64 = seed;
    let mut next = || {
        rng_state = rng_state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        rng_state
    };

    let mut sizes = Vec::with_capacity(num_files);
    let mut total = 0usize;
    for i in 0..num_files {
        let r = (next() >> 32) as u32;
        let bucket = r % 100;
        // 60% near average, 25% half, 10% 2x, 5% 4x
        let size = if bucket < 60 {
            avg
        } else if bucket < 85 {
            avg / 2
        } else if bucket < 95 {
            avg * 2
        } else {
            avg * 4
        }
        .max(512);
        sizes.push(size);
        total += size;
        // Leave a "tail" to balance: last file absorbs remainder so total matches.
        if i + 1 == num_files && total < total_bytes {
            sizes[i] += total_bytes - total;
        }
    }

    sizes
        .into_iter()
        .enumerate()
        .map(|(idx, size)| {
            // Random-ish bytes; Defender treats high-entropy content the same.
            // Use a cheap fill so workload-generation isn't the bottleneck.
            let mut buf = vec![0u8; size];
            let mut s = seed.wrapping_add(idx as u64).wrapping_add(1);
            for b in buf.chunks_mut(8) {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let bytes = s.to_le_bytes();
                for (dst, src) in b.iter_mut().zip(bytes.iter()) {
                    *dst = *src;
                }
            }
            // Split the buffer into N roughly-equal payloads. N=1 is a
            // single-output compile (a .o); N=3 mimics rustc's
            // rlib + rmeta + .d set.
            let n = payloads_per_job();
            let payloads: Vec<Arc<Vec<u8>>> = if n == 1 {
                vec![Arc::new(buf)]
            } else {
                let mut out: Vec<Vec<u8>> = Vec::with_capacity(n);
                let chunk = buf.len() / n;
                let mut rem = buf;
                for _ in 0..n - 1 {
                    let tail = rem.split_off(chunk);
                    out.push(rem);
                    rem = tail;
                }
                out.push(rem);
                out.into_iter().map(Arc::new).collect()
            };
            let key_hex = format!("{idx:016x}{seed:016x}{idx:032x}");
            let payload_sizes: Vec<u64> = payloads.iter().map(|p| p.len() as u64).collect();
            let output_names: Vec<String> =
                (0..payloads.len()).map(|i| format!("out_{i}.o")).collect();
            Job {
                key_hex,
                payloads,
                meta: ArtifactIndex::new(
                    output_names,
                    payload_sizes,
                    Vec::<u8>::new(),
                    Vec::<u8>::new(),
                    0,
                ),
            }
        })
        .collect()
}

/// If `ZCCACHE_BENCH_ROOT` is set, place fresh per-trial dirs under it; else use TEMP.
/// Defender's behaviour can depend on path (some TEMP/AppData paths get less-deep
/// scanning than user dirs), so we expose this as a knob for honest measurement.
fn fresh_artifact_dir() -> TempDir {
    let mut b = tempfile::Builder::new();
    b.prefix("zccache-persist-bench-");
    if let Ok(root) = std::env::var("ZCCACHE_BENCH_ROOT") {
        let root = PathBuf::from(root);
        std::fs::create_dir_all(&root).expect("create bench root");
        b.tempdir_in(&root).expect("tempdir in root")
    } else {
        b.tempdir().expect("tempdir")
    }
}

fn artifact_filename(key_hex: &str, idx: usize) -> String {
    if std::env::var("ZCCACHE_BENCH_REAL_EXTS").is_ok() {
        // Mimic the rmeta/rlib/o mix that triggers Defender's PE/COFF scanning.
        let ext = match idx % 3 {
            0 => "rlib",
            1 => "rmeta",
            _ => "o",
        };
        format!("{key_hex}_{idx}.{ext}")
    } else {
        // Match the daemon's actual filename format today.
        format!("{key_hex}_{idx}")
    }
}

fn open_store(dir: &Path) -> Arc<ArtifactStore> {
    let store_path = dir.join("index.redb");
    Arc::new(ArtifactStore::open(&store_path).expect("open redb store"))
}

/// Pack header layout for `.pack` files:
///
///   [magic: 4 bytes = b"ZCPK"]
///   [num_payloads: u32 le]
///   [(offset: u64 le, size: u64 le)] * num_payloads
///   [payload_0 bytes]
///   [payload_1 bytes]
///   ...
///
/// Defender's per-file scan cost is amortized over all payloads, so a single
/// 3 MB pack with 3 sub-payloads is dramatically cheaper to land than 3
/// separate 1 MB files.
fn build_pack(payloads: &[Arc<Vec<u8>>]) -> Vec<u8> {
    let n = payloads.len();
    let header_size = 4 + 4 + n * 16;
    let total: usize = header_size + payloads.iter().map(|p| p.len()).sum::<usize>();
    let mut buf = Vec::with_capacity(total);
    buf.extend_from_slice(b"ZCPK");
    buf.extend_from_slice(&(n as u32).to_le_bytes());
    let mut offset = header_size as u64;
    for p in payloads {
        buf.extend_from_slice(&offset.to_le_bytes());
        buf.extend_from_slice(&(p.len() as u64).to_le_bytes());
        offset += p.len() as u64;
    }
    for p in payloads {
        buf.extend_from_slice(p);
    }
    buf
}

/// Strategy A: today's behaviour. semaphore=8, serial writes within task,
/// redb insert inside the spawn_blocking under the semaphore.
async fn run_strategy_serial_in_task(
    artifact_dir: PathBuf,
    store: Arc<ArtifactStore>,
    jobs: Vec<Job>,
    semaphore_size: usize,
) {
    let sem = Arc::new(tokio::sync::Semaphore::new(semaphore_size));
    let mut handles = Vec::with_capacity(jobs.len());
    for job in jobs {
        let sem = Arc::clone(&sem);
        let store = Arc::clone(&store);
        let dir = artifact_dir.clone();
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            tokio::task::spawn_blocking(move || {
                for (i, payload) in job.payloads.iter().enumerate() {
                    let p = dir.join(artifact_filename(&job.key_hex, i));
                    let _ = std::fs::write(&p, &**payload);
                }
                store.insert(&job.key_hex, &job.meta);
            })
            .await
            .ok();
        }));
    }
    for h in handles {
        let _ = h.await;
    }
}

/// Strategy B: fan out each payload as its own spawn_blocking. The semaphore
/// caps total in-flight writes, not "tasks". redb insert still inside, but
/// only one of the payloads (the last) carries the insert so we don't
/// duplicate it.
async fn run_strategy_fanout_payloads(
    artifact_dir: PathBuf,
    store: Arc<ArtifactStore>,
    jobs: Vec<Job>,
    semaphore_size: usize,
) {
    let sem = Arc::new(tokio::sync::Semaphore::new(semaphore_size));
    let mut handles = Vec::new();
    for job in jobs {
        let store = Arc::clone(&store);
        let dir = artifact_dir.clone();
        let key = job.key_hex.clone();
        let meta = job.meta.clone();

        // Spawn one write task per payload.
        let payload_count = job.payloads.len();
        let mut payload_handles = Vec::with_capacity(payload_count);
        for (i, payload) in job.payloads.into_iter().enumerate() {
            let sem = Arc::clone(&sem);
            let p = dir.join(artifact_filename(&key, i));
            payload_handles.push(tokio::spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                tokio::task::spawn_blocking(move || {
                    let _ = std::fs::write(&p, &*payload);
                })
                .await
                .ok();
            }));
        }
        // After all payloads land, commit the redb row. Off the disk-write semaphore.
        handles.push(tokio::spawn(async move {
            for h in payload_handles {
                let _ = h.await;
            }
            let store2 = Arc::clone(&store);
            let key2 = key.clone();
            let meta2 = meta.clone();
            tokio::task::spawn_blocking(move || {
                store2.insert(&key2, &meta2);
            })
            .await
            .ok();
        }));
    }
    for h in handles {
        let _ = h.await;
    }
}

/// Strategy D: today's serial-within-task disk writes (semaphore-gated), but
/// redb commits flow through a separate single-writer task fed by an unbounded
/// channel. Mirrors the new implementation in `server.rs` after iteration 2.
async fn run_strategy_serial_in_task_async_redb(
    artifact_dir: PathBuf,
    store: Arc<ArtifactStore>,
    jobs: Vec<Job>,
    semaphore_size: usize,
) {
    let sem = Arc::new(tokio::sync::Semaphore::new(semaphore_size));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, ArtifactIndex)>();

    let store_for_writer = Arc::clone(&store);
    let writer = tokio::task::spawn_blocking(move || {
        let mut buf: Vec<(String, ArtifactIndex)> = Vec::with_capacity(64);
        loop {
            buf.clear();
            match rx.blocking_recv() {
                Some(item) => buf.push(item),
                None => return,
            }
            while buf.len() < 64 {
                match rx.try_recv() {
                    Ok(item) => buf.push(item),
                    Err(_) => break,
                }
            }
            for (k, m) in buf.drain(..) {
                store_for_writer.insert(&k, &m);
            }
        }
    });

    let mut handles = Vec::with_capacity(jobs.len());
    for job in jobs {
        let sem = Arc::clone(&sem);
        let tx = tx.clone();
        let dir = artifact_dir.clone();
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            let written = tokio::task::spawn_blocking(move || {
                for (i, payload) in job.payloads.iter().enumerate() {
                    let p = dir.join(artifact_filename(&job.key_hex, i));
                    let _ = std::fs::write(&p, &**payload);
                }
                (job.key_hex, job.meta)
            })
            .await;
            if let Ok((k, m)) = written {
                let _ = tx.send((k, m));
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    drop(tx);
    let _ = writer.await;
}

/// Strategy E: packed write — concatenate every payload of a job into a
/// single `.pack` file (one `std::fs::write` per job instead of N), plus the
/// same async redb channel as Strategy D. Goal: collapse Defender's
/// per-file scan overhead, which is the *unit* of the slowdown on Windows.
async fn run_strategy_packed_async_redb(
    artifact_dir: PathBuf,
    store: Arc<ArtifactStore>,
    jobs: Vec<Job>,
    semaphore_size: usize,
) {
    let sem = Arc::new(tokio::sync::Semaphore::new(semaphore_size));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, ArtifactIndex)>();

    let store_for_writer = Arc::clone(&store);
    let writer = tokio::task::spawn_blocking(move || {
        let mut buf: Vec<(String, ArtifactIndex)> = Vec::with_capacity(64);
        loop {
            buf.clear();
            match rx.blocking_recv() {
                Some(item) => buf.push(item),
                None => return,
            }
            while buf.len() < 64 {
                match rx.try_recv() {
                    Ok(item) => buf.push(item),
                    Err(_) => break,
                }
            }
            for (k, m) in buf.drain(..) {
                store_for_writer.insert(&k, &m);
            }
        }
    });

    let mut handles = Vec::with_capacity(jobs.len());
    for job in jobs {
        let sem = Arc::clone(&sem);
        let tx = tx.clone();
        let dir = artifact_dir.clone();
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            let written = tokio::task::spawn_blocking(move || {
                let packed = build_pack(&job.payloads);
                let p = dir.join(format!("{}.pack", job.key_hex));
                let _ = std::fs::write(&p, &packed);
                (job.key_hex, job.meta)
            })
            .await;
            if let Ok((k, m)) = written {
                let _ = tx.send((k, m));
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    drop(tx);
    let _ = writer.await;
}

/// Strategy C: fanout + batched redb commits (single writer task drains a
/// channel of completed jobs and inserts them in batches of 32).
async fn run_strategy_fanout_batched_redb(
    artifact_dir: PathBuf,
    store: Arc<ArtifactStore>,
    jobs: Vec<Job>,
    semaphore_size: usize,
) {
    let sem = Arc::new(tokio::sync::Semaphore::new(semaphore_size));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, ArtifactIndex)>();

    let store_for_writer = Arc::clone(&store);
    let writer = tokio::task::spawn_blocking(move || {
        let mut buf: Vec<(String, ArtifactIndex)> = Vec::with_capacity(64);
        loop {
            buf.clear();
            // Block on first item.
            match rx.blocking_recv() {
                Some(item) => buf.push(item),
                None => return,
            }
            // Drain non-blocking up to 64.
            while buf.len() < 64 {
                match rx.try_recv() {
                    Ok(item) => buf.push(item),
                    Err(_) => break,
                }
            }
            for (k, m) in buf.drain(..) {
                store_for_writer.insert(&k, &m);
            }
        }
    });

    let mut handles = Vec::new();
    for job in jobs {
        let dir = artifact_dir.clone();
        let key = job.key_hex.clone();
        let meta = job.meta.clone();
        let tx = tx.clone();

        let mut payload_handles = Vec::with_capacity(job.payloads.len());
        for (i, payload) in job.payloads.into_iter().enumerate() {
            let sem = Arc::clone(&sem);
            let p = dir.join(artifact_filename(&key, i));
            payload_handles.push(tokio::spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                tokio::task::spawn_blocking(move || {
                    let _ = std::fs::write(&p, &*payload);
                })
                .await
                .ok();
            }));
        }
        handles.push(tokio::spawn(async move {
            for h in payload_handles {
                let _ = h.await;
            }
            let _ = tx.send((key, meta));
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    drop(tx);
    let _ = writer.await;
}

#[derive(Debug, serde::Serialize)]
struct Row {
    strategy: String,
    semaphore: usize,
    files: usize,
    bytes: u64,
    wall_ms_min: u64,
    wall_ms_median: u64,
    wall_ms_max: u64,
    files_per_sec_median: f64,
    mb_per_sec_median: f64,
}

fn print_table(rows: &[Row]) {
    println!(
        "{:<32} {:>5} {:>5} {:>10} {:>8} {:>8} {:>8} {:>10} {:>9}",
        "strategy", "sem", "files", "bytes", "min_ms", "med_ms", "max_ms", "files/s", "MB/s"
    );
    println!("{}", "-".repeat(100));
    for r in rows {
        println!(
            "{:<32} {:>5} {:>5} {:>10} {:>8} {:>8} {:>8} {:>10.0} {:>9.1}",
            r.strategy,
            r.semaphore,
            r.files,
            r.bytes,
            r.wall_ms_min,
            r.wall_ms_median,
            r.wall_ms_max,
            r.files_per_sec_median,
            r.mb_per_sec_median
        );
    }
}

fn summarize(samples: &[u64]) -> (u64, u64, u64) {
    let mut s = samples.to_vec();
    s.sort_unstable();
    let min = *s.first().unwrap_or(&0);
    let max = *s.last().unwrap_or(&0);
    let median = if s.is_empty() {
        0
    } else if s.len() % 2 == 1 {
        s[s.len() / 2]
    } else {
        (s[s.len() / 2 - 1] + s[s.len() / 2]) / 2
    };
    (min, median, max)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn persist_pool_bench() {
    let num_files: usize = std::env::var("PERSIST_BENCH_FILES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(191);
    let total_mb: usize = std::env::var("PERSIST_BENCH_TOTAL_MB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(327);
    let trials: usize = std::env::var("PERSIST_BENCH_TRIALS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    let total_bytes = total_mb * 1024 * 1024;
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    println!("workload: {num_files} files, total {total_mb} MB, cpus={cpus}, trials={trials}");

    // Strategies and semaphore sizes to evaluate.
    let cases: Vec<(&str, &str, usize)> = vec![
        ("baseline (today)", "serial_in_task", 8),
        ("bigger sem", "serial_in_task", 16),
        ("bigger sem", "serial_in_task", 32),
        ("bigger sem", "serial_in_task", 64),
        ("bigger sem", "serial_in_task", 128),
        ("fanout payloads", "fanout_payloads", 32),
        ("fanout payloads", "fanout_payloads", 64),
        ("fanout payloads", "fanout_payloads", 128),
        ("fanout + batched redb", "fanout_batched_redb", 32),
        ("fanout + batched redb", "fanout_batched_redb", 64),
        ("fanout + batched redb", "fanout_batched_redb", 128),
        ("serial + async redb (new)", "serial_in_task_async_redb", 8),
        ("serial + async redb (new)", "serial_in_task_async_redb", 16),
        ("serial + async redb (new)", "serial_in_task_async_redb", 32),
        ("packed + async redb", "packed_async_redb", 8),
        ("packed + async redb", "packed_async_redb", 16),
        ("packed + async redb", "packed_async_redb", 32),
    ];

    let mut rows = Vec::new();
    for (label, strat, sem) in &cases {
        let mut wall_samples: Vec<u64> = Vec::with_capacity(trials);
        let mut bytes_total: u64 = 0;
        for trial in 0..trials {
            // Fresh workload + fresh artifact dir per trial to avoid OS page cache
            // and per-file dedup effects.
            let jobs = make_workload(num_files, total_bytes, (trial as u64) * 17 + 1);
            let bytes: u64 = jobs
                .iter()
                .map(|j| j.payloads.iter().map(|p| p.len() as u64).sum::<u64>())
                .sum();
            let dir = fresh_artifact_dir();
            let store = open_store(dir.path());
            let artifact_dir = dir.path().join("artifacts");
            std::fs::create_dir_all(&artifact_dir).unwrap();

            let t0 = Instant::now();
            match *strat {
                "serial_in_task" => {
                    run_strategy_serial_in_task(
                        artifact_dir.clone(),
                        Arc::clone(&store),
                        jobs,
                        *sem,
                    )
                    .await;
                }
                "fanout_payloads" => {
                    run_strategy_fanout_payloads(
                        artifact_dir.clone(),
                        Arc::clone(&store),
                        jobs,
                        *sem,
                    )
                    .await;
                }
                "fanout_batched_redb" => {
                    run_strategy_fanout_batched_redb(
                        artifact_dir.clone(),
                        Arc::clone(&store),
                        jobs,
                        *sem,
                    )
                    .await;
                }
                "serial_in_task_async_redb" => {
                    run_strategy_serial_in_task_async_redb(
                        artifact_dir.clone(),
                        Arc::clone(&store),
                        jobs,
                        *sem,
                    )
                    .await;
                }
                "packed_async_redb" => {
                    run_strategy_packed_async_redb(
                        artifact_dir.clone(),
                        Arc::clone(&store),
                        jobs,
                        *sem,
                    )
                    .await;
                }
                _ => unreachable!(),
            }
            let elapsed = t0.elapsed();
            wall_samples.push(elapsed.as_millis() as u64);
            bytes_total += bytes;
            // Drop tempdir AFTER timing — cleanup cost is not counted.
            drop(store);
            drop(dir);
        }
        let (wmin, wmed, wmax) = summarize(&wall_samples);
        let bytes = bytes_total / trials as u64;
        rows.push(Row {
            strategy: (*label).to_string(),
            semaphore: *sem,
            files: num_files,
            bytes,
            wall_ms_min: wmin,
            wall_ms_median: wmed,
            wall_ms_max: wmax,
            files_per_sec_median: (num_files as f64) / (wmed as f64 / 1000.0).max(1e-6),
            mb_per_sec_median: (bytes as f64 / 1_048_576.0) / (wmed as f64 / 1000.0).max(1e-6),
        });
    }

    print_table(&rows);

    if let Ok(path) = std::env::var("PERSIST_BENCH_JSON") {
        let json = serde_json::to_string_pretty(&rows).unwrap();
        std::fs::write(&path, json).unwrap();
        println!("wrote results to {path}");
    }
}

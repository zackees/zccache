use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};

fn bench_hash_bytes(c: &mut Criterion) {
    let data_1kb = vec![0x42u8; 1024];
    let data_1mb = vec![0x42u8; 1024 * 1024];
    let data_64mb = vec![0x42u8; 64 * 1024 * 1024];

    let mut group = c.benchmark_group("blake3_hash");

    group.throughput(Throughput::Bytes(1024));
    group.bench_function("1KB", |b| {
        b.iter(|| zccache_monocrate::hash::hash_bytes(black_box(&data_1kb)));
    });

    group.throughput(Throughput::Bytes(1024 * 1024));
    group.bench_function("1MB", |b| {
        b.iter(|| zccache_monocrate::hash::hash_bytes(black_box(&data_1mb)));
    });

    group.throughput(Throughput::Bytes(64 * 1024 * 1024));
    group.bench_function("64MB", |b| {
        b.iter(|| zccache_monocrate::hash::hash_bytes(black_box(&data_64mb)));
    });

    group.finish();
}

criterion_group!(benches, bench_hash_bytes);
criterion_main!(benches);

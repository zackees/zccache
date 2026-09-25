//! Criterion bench for `scan_includes_str` on a synthetic header corpus.
//!
//! `scan_includes_str` is the per-file `#include` extractor run for every
//! header visited by `scan_recursive` on a compile cache MISS (see
//! crates/zccache-depgraph/src/scanner.rs). The corpus is generated in
//! memory to resemble avr-libc + ArduinoCore headers: ~60 headers of ~300
//! lines each, dominated by declarations, `/** ... */` doc comments, `//`
//! comments, multi-line `#define` macros with backslash continuations,
//! `#ifdef` guards, and 5-15 `#include <...>` / `"..."` lines per header.
//!
//! Throughput is reported as `Throughput::Bytes` over the concatenated corpus.
//!
//! Acceptance target (zackees/zccache#1670): the single-pass scanner must be
//! >=5x faster than the pre-change scanner. Save a baseline on the pre-change
//! commit with `-- --save-baseline pre`, then re-run after the impl change
//! with `-- --baseline pre`.
//!
//! Run with: `soldr cargo bench -p zccache --bench scan_includes`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::fmt::Write as _;

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use zccache::depgraph::scanner::scan_includes_str;

const HEADER_COUNT: usize = 60;
const TARGET_LINES: usize = 300;

/// Generate one synthetic avr-libc / ArduinoCore-style header.
fn build_header(id: usize) -> String {
    let mut s = String::with_capacity(TARGET_LINES * 48);
    let guard = format!("_SYNTH_HEADER_{id:03}_H_");
    let _ = writeln!(s, "/* Copyright (c) 2002-2007 Synthetic Authors");
    let _ = writeln!(s, "   All rights reserved. */");
    let _ = writeln!(s, "#ifndef {guard}");
    let _ = writeln!(s, "#define {guard} 1");
    let _ = writeln!(s);

    // 5-15 includes, mixing system and quoted forms.
    let n_includes = 5 + (id * 7) % 11;
    for k in 0..n_includes {
        if k % 2 == 0 {
            let _ = writeln!(s, "#include <avr/sys_{:03}.h>", (id + k) % HEADER_COUNT);
        } else {
            let _ = writeln!(s, "#include \"core_{:03}.h\"", (id + k) % HEADER_COUNT);
        }
    }
    let _ = writeln!(s);

    let mut block = 0usize;
    while s.lines().count() < TARGET_LINES {
        let _ = writeln!(s, "/**");
        let _ = writeln!(s, " * \\brief Synthetic API number {block} for header {id}.");
        let _ = writeln!(s, " *");
        let _ = writeln!(s, " * The #include <not_real.h> text inside a comment must be skipped.");
        let _ = writeln!(s, " */");
        let _ = writeln!(s, "#ifdef __AVR_FEATURE_{block}__");
        let _ = writeln!(s, "#define SYNTH_MACRO_{id}_{block}(x, y) \\");
        let _ = writeln!(s, "    do {{ \\");
        let _ = writeln!(s, "        (x) = (y) + {block}; \\");
        let _ = writeln!(s, "    }} while (0)");
        let _ = writeln!(s, "#endif // __AVR_FEATURE_{block}__");
        let _ = writeln!(s, "// Register accessor for block {block}");
        let _ = writeln!(s, "extern uint8_t synth_reg_{id}_{block}(uint16_t addr, uint8_t mask);");
        let _ = writeln!(s, "extern void synth_write_{id}_{block}(uint16_t addr, uint8_t v);");
        let _ = writeln!(s, "static inline int synth_inline_{id}_{block}(int a) {{ return a * {block}; }}");
        let _ = writeln!(s, "typedef struct {{ uint8_t lo; uint8_t hi; }} synth_pair_{id}_{block}_t;");
        let _ = writeln!(s);
        block += 1;
    }

    let _ = writeln!(s, "#endif /* {guard} */");
    s
}

fn build_corpus() -> Vec<String> {
    (0..HEADER_COUNT).map(build_header).collect()
}

fn bench_scan_includes(c: &mut Criterion) {
    let corpus = build_corpus();
    let total_bytes: usize = corpus.iter().map(String::len).sum();

    let mut group = c.benchmark_group("scan_includes");
    group.throughput(Throughput::Bytes(total_bytes as u64));
    group.bench_function(format!("corpus_{HEADER_COUNT}_headers"), |b| {
        b.iter(|| {
            for header in black_box(&corpus) {
                let r = scan_includes_str(black_box(header.as_str()));
                black_box(r);
            }
        });
    });
    group.finish();
}

criterion_group!(benches, bench_scan_includes);
criterion_main!(benches);

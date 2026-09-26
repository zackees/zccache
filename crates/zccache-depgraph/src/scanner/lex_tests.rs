//! Tests for the single-pass lexer (zccache#1670).
//!
//! `legacy` is the previous multi-pass scanner, kept verbatim as a
//! reference: the new lexer must find the same directives on ordinary
//! input, and must be faster.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::{scan_includes_str, IncludeDirective, IncludeKind};
use std::time::{Duration, Instant};

/// The scanner as it was before zccache#1670.
mod legacy {
    use super::super::{parse_include_from_line, IncludeDirective};

    pub(super) fn scan_includes_str(source: &str) -> Vec<IncludeDirective> {
        let joined = join_continuations(source);
        let mut results = Vec::new();

        // Track original line numbers: each line in `joined` maps to a source line.
        // After joining continuations, we need to track the starting line of each
        // logical line.
        let line_map = build_line_map(source);

        let mut in_block_comment = false;

        for (logical_idx, line) in joined.lines().enumerate() {
            let source_line = if logical_idx < line_map.len() {
                line_map[logical_idx]
            } else {
                (logical_idx + 1) as u32
            };

            if in_block_comment {
                if let Some(end) = line.find("*/") {
                    // Block comment ends on this line. Check rest of line.
                    let rest = &line[end + 2..];
                    if let Some(dir) = parse_include_from_line(rest) {
                        results.push(IncludeDirective {
                            line: source_line,
                            ..dir
                        });
                    }
                    in_block_comment = false;
                    // Could have another block comment start after — fuse the
                    // detect-and-locate into one search to drop the expect and
                    // halve the scanner's per-line work on the hot path.
                    if let Some(after_end) = rest.find("/*") {
                        if !rest[..after_end].contains("*/") {
                            in_block_comment = true;
                        }
                    }
                }
                continue;
            }

            // Strip line comments first.
            let effective = strip_comments(line, &mut in_block_comment);
            if let Some(dir) = parse_include_from_line(&effective) {
                results.push(IncludeDirective {
                    line: source_line,
                    ..dir
                });
            }
        }

        results
    }

    /// Join backslash-continued lines into single logical lines.
    fn join_continuations(source: &str) -> String {
        let mut result = String::with_capacity(source.len());
        let mut chars = source.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '\\' {
                match chars.peek() {
                    Some('\n') => {
                        // Don't emit either the backslash or the newline.
                        chars.next();
                    }
                    Some('\r') => {
                        chars.next(); // consume \r
                        if chars.peek() == Some(&'\n') {
                            chars.next(); // consume \n
                        }
                        // Don't emit.
                    }
                    _ => result.push(ch),
                }
            } else {
                result.push(ch);
            }
        }

        result
    }

    /// Build a map from logical line index to 1-based source line number.
    /// Accounts for backslash continuations merging multiple source lines.
    fn build_line_map(source: &str) -> Vec<u32> {
        let mut map = Vec::new();
        let mut continued = false;

        for (source_line, line) in (1_u32..).zip(source.split('\n')) {
            if !continued {
                map.push(source_line);
            }
            let trimmed = line.trim_end_matches('\r');
            continued = trimmed.ends_with('\\');
        }

        map
    }

    /// Strip line comments and block comments from a line.
    /// Updates `in_block_comment` state for multi-line block comments.
    ///
    /// String literals are NOT stripped. This is intentional:
    /// `#include "foo.h"` has quotes that look like strings but are part of
    /// the directive syntax. False positives like `const char* s = "#include ..."`
    /// are handled by `parse_include_from_line` which requires `#` to be the
    /// first non-whitespace character on the line.
    fn strip_comments(line: &str, in_block_comment: &mut bool) -> String {
        let mut result = String::with_capacity(line.len());
        let bytes = line.as_bytes();
        let len = bytes.len();
        let mut i = 0;

        while i < len {
            if *in_block_comment {
                if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    *in_block_comment = false;
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }

            // Line comment â€” stop processing this line.
            if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'/' {
                break;
            }

            // Block comment start.
            if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                *in_block_comment = true;
                i += 2;
                continue;
            }

            result.push(bytes[i] as char);
            i += 1;
        }

        result
    }
}

/// Header-shaped fragments covering comments, splices, CRLF and
/// non-include directives. None of them hits a legacy bug, so both
/// scanners must agree on any concatenation of them.
const FRAGMENTS: &[&str] = &[
    "#include <a.h>\n",
    "#include \"b/c.h\"\n",
    "  #  include <d.h>\n",
    "#include_next <e.h>\n",
    "#include MACRO_H\n",
    "#define X 1\n",
    "#define LONG(a) \\\n  do { a; } while (0)\n",
    "#if defined(FOO) && FOO > 1\n",
    "#endif /* FOO */\n",
    "int x = 1; // #include <no1.h>\n",
    "/* #include <no2.h> */\n",
    "/*\n * block\n#include <no3.h>\n */\n",
    "/* c */ #include <f.h>\n",
    "#include <g.h> // trailing\n",
    "#include <h.h> /* trailing */\n",
    "#in\\\nclude \"i.h\"\n",
    "const char *s = \"#include <no4.h>\";\n",
    "code(); /* open\n still\n*/ #include <j.h>\n",
    "#include <k.h>\r\n",
    "\r\n",
    "x = a / b * c; y = *p / 2;\n",
    "#include <m.h> /* open\n */\n",
    "#/**/include <n.h>\n",
    "// line comment \\\n #include <no5.h>\n",
    "static inline int f(int v) { return v * 2; }\n",
    "\n",
    " \t\n",
];

/// Deterministic pseudo-random concatenation of `FRAGMENTS`.
fn synthetic_source(seed: u64, fragments: usize) -> String {
    let mut state = seed;
    let mut out = String::new();
    for _ in 0..fragments {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        out.push_str(FRAGMENTS[(state >> 33) as usize % FRAGMENTS.len()]);
    }
    out
}

fn paths(directives: &[IncludeDirective]) -> Vec<(&str, u32)> {
    directives
        .iter()
        .map(|d| (d.path.as_str(), d.line))
        .collect()
}

#[test]
fn matches_legacy_on_every_fragment() {
    for fragment in FRAGMENTS {
        assert_eq!(
            scan_includes_str(fragment),
            legacy::scan_includes_str(fragment),
            "fragment {fragment:?}"
        );
    }
}

#[test]
fn matches_legacy_on_synthetic_headers() {
    for seed in 0..200 {
        let source = synthetic_source(seed, 60);
        assert_eq!(
            scan_includes_str(&source),
            legacy::scan_includes_str(&source),
            "seed {seed}:\n{source}"
        );
    }
}

#[test]
fn directive_after_comment_that_reopens_on_the_same_line() {
    // The legacy scanner left the rest of this line unstripped and then
    // wrongly stayed inside a block comment, dropping both includes.
    let source = "/* a\n*/ /* b */ #include <x.h>\n#include <y.h>\n";
    assert_eq!(paths(&scan_includes_str(source)), [("x.h", 2), ("y.h", 3)]);
    assert!(legacy::scan_includes_str(source).is_empty());
}

#[test]
fn leading_byte_order_mark_is_skipped() {
    let includes = scan_includes_str("\u{feff}#include <a.h>\n#include <b.h>\n");
    assert_eq!(paths(&includes), [("a.h", 1), ("b.h", 2)]);
}

#[test]
fn non_ascii_header_name_is_preserved() {
    let includes = scan_includes_str("#include \"caf\u{e9}.h\"\n");
    assert_eq!(includes[0].path, "caf\u{e9}.h");
    assert_eq!(includes[0].kind, IncludeKind::Quoted);
}

#[test]
fn splice_inside_comment_delimiters() {
    let source = "/\\\n* hidden #include <no.h> *\\\n/ #include <a.h>\n#include <b.h>\n";
    assert_eq!(paths(&scan_includes_str(source)), [("a.h", 1), ("b.h", 4)]);
}

#[test]
fn line_comment_continues_across_splice() {
    let source = "// note \\\n#include <no.h>\n#include <a.h>\n";
    assert_eq!(paths(&scan_includes_str(source)), [("a.h", 3)]);
}

#[test]
fn unterminated_block_comment_hides_the_rest() {
    let source = "#include <a.h>\n/* never closed\n#include <no.h>\n";
    assert_eq!(paths(&scan_includes_str(source)), [("a.h", 1)]);
}

#[test]
fn directives_that_cannot_be_include_are_skipped_cheaply() {
    let source = "#define A /* open\n #include <no.h> */ 1\n#ifdef A\n#include <a.h>\n#endif\n";
    assert_eq!(paths(&scan_includes_str(source)), [("a.h", 4)]);
}

/// One timing of `scan` over `sources`.
fn time_once(sources: &[String], scan: fn(&str) -> Vec<IncludeDirective>) -> Duration {
    let start = Instant::now();
    for source in sources {
        std::hint::black_box(scan(std::hint::black_box(source)));
    }
    start.elapsed()
}

/// Fastest of `rounds` timings of the new and legacy scanners, with rounds
/// interleaved so machine-load swings hit both alike.
fn best_times(sources: &[String], rounds: usize) -> (Duration, Duration) {
    (0..rounds).fold((Duration::MAX, Duration::MAX), |(new, old), _| {
        (
            new.min(time_once(sources, scan_includes_str)),
            old.min(time_once(sources, legacy::scan_includes_str)),
        )
    })
}

/// A header shaped like avr-libc's `io*.h`: mostly register `#define`s,
/// doc comments and declarations, with a few includes.
fn header_like_source(seed: u64) -> String {
    let mut out =
        String::from("/* Copyright notice\n * spanning lines\n */\n#ifndef H\n#define H\n");
    for block in 0..40 {
        out.push_str(&format!("#include <avr/sfr_{seed}_{block}.h>\n"));
        out.push_str("/** Register group.\n    Documented at length. */\n");
        for reg in 0..8 {
            out.push_str(&format!(
                "#define REG_{block}_{reg} _SFR_MEM8(0x{:02X})  /* bit {reg} */\n",
                block * 8 + reg
            ));
        }
        out.push_str("extern void handler(unsigned char value); // ISR hook\n\n");
    }
    out.push_str("#endif /* H */\n");
    out
}

/// Minimum speedup over the legacy scanner on `header_like_source`.
///
/// Optimized builds measure ~5x here and ~8x on avr-libc + ArduinoCore
/// (`matches_legacy_on_corpus_dir`). Unoptimized test builds (what CI runs)
/// leave `memchr` and the lexer's small helpers un-inlined and measure ~1.5x,
/// so the debug floor only catches a fall back to legacy-class speed.
const MIN_SPEEDUP: f64 = if cfg!(debug_assertions) { 1.1 } else { 3.0 };

/// Perf unit test (zccache#1670): the single-pass lexer must stay well
/// ahead of the legacy scanner on header-shaped input.
#[test]
fn single_pass_lexer_is_faster_than_legacy() {
    let sources: Vec<String> = (0..40).map(header_like_source).collect();
    for source in &sources {
        assert_eq!(scan_includes_str(source), legacy::scan_includes_str(source));
    }
    let (new, old) = best_times(&sources, 7);
    let speedup = old.as_secs_f64() / new.as_secs_f64();
    eprintln!("single-pass {new:?}, legacy {old:?}, speedup {speedup:.2}x");
    assert!(
        speedup >= MIN_SPEEDUP,
        "single-pass lexer {new:?} is only {speedup:.2}x faster than legacy {old:?} (floor {MIN_SPEEDUP}x)"
    );
}

/// Differential check and speed comparison against a real header tree,
/// e.g. avr-libc plus ArduinoCore (the zccache#1670 acceptance set):
/// `ZCCACHE_SCAN_CORPUS_DIR=<dir> soldr cargo test --release -p
/// zccache-depgraph --lib matches_legacy_on_corpus_dir -- --ignored --nocapture`.
/// Several directories can be joined with the platform path separator.
#[test]
#[ignore = "needs ZCCACHE_SCAN_CORPUS_DIR pointing at a C/C++ header tree"]
fn matches_legacy_on_corpus_dir() {
    let roots = std::env::var_os("ZCCACHE_SCAN_CORPUS_DIR").expect("ZCCACHE_SCAN_CORPUS_DIR");
    let mut stack: Vec<_> = std::env::split_paths(&roots).collect();
    let mut sources = Vec::new();
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(source) = std::fs::read_to_string(&path) {
                assert_eq!(
                    scan_includes_str(&source),
                    legacy::scan_includes_str(&source),
                    "{}",
                    path.display()
                );
                sources.push(source);
            }
        }
    }
    assert!(
        !sources.is_empty(),
        "no readable files under the corpus dir"
    );
    let bytes: usize = sources.iter().map(String::len).sum();
    let (new, old) = best_times(&sources, 15);
    let speedup = old.as_secs_f64() / new.as_secs_f64();
    eprintln!(
        "corpus: {} files, {} KiB; single-pass {new:?}, legacy {old:?}, speedup {speedup:.1}x",
        sources.len(),
        bytes / 1024,
    );
    // zccache#1670 acceptance: >= 5x on avr-libc + ArduinoCore, optimized.
    if !cfg!(debug_assertions) {
        assert!(speedup >= 5.0, "corpus speedup {speedup:.1}x is below 5x");
    }
}

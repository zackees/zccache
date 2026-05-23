//! Adversarial stress tests for response file parsing and expansion.
//!
//! Targets every dark corner of the `@file` implementation:
//! - Quoting edge cases (unterminated, mixed, nested-looking, adjacent)
//! - Escape sequences (boundary, unknown, chained, EOF)
//! - Whitespace (exotic, mixed line endings, BOM)
//! - Large inputs (long args, many args, deep nesting)
//! - Expansion depth/cycle boundaries (exact MAX_DEPTH, off-by-one)
//! - Circular reference variants (self, mutual, triangle, deep-cycle)
//! - File content edge cases (empty, whitespace-only, BOM, CRLF)
//! - Integration with `parse_invocation` through response files
//!
//! Run all:    soldr cargo test -p zccache-compiler --test response_file_adversarial -- --nocapture
//! Run single: soldr cargo test -p zccache-compiler --test response_file_adversarial -- <test_name> --nocapture

use std::path::Path;
use zccache_compiler::response_file::{
    expand_response_files, parse_response_file_content, ResponseFileError,
};
use zccache_monocrate::core::NormalizedPath;

#[cfg(windows)]
use zccache_compiler::response_file::write_response_file_if_needed;

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[cfg(windows)]
fn force_spill_args_owned(seed: Vec<String>) -> Vec<String> {
    let mut args = seed;
    while args.iter().map(|a| a.len() + 3).sum::<usize>() < 31_000 {
        args.push(format!("-D_FILLER_{}={}", args.len(), "X".repeat(128)));
    }
    args
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 1: PARSER ADVERSARIAL — QUOTING EDGE CASES
// ═══════════════════════════════════════════════════════════════════════════════

/// Unterminated double quote: parser should treat rest of input as quoted content.
#[test]
fn parse_unterminated_double_quote() {
    // `"hello` — no closing quote. Parser reads to EOF inside quote loop.
    let result = parse_response_file_content("\"hello");
    assert_eq!(result, s(&["hello"]));
}

/// Unterminated single quote: parser should treat rest of input as quoted content.
#[test]
fn parse_unterminated_single_quote() {
    let result = parse_response_file_content("'hello");
    assert_eq!(result, s(&["hello"]));
}

/// Unterminated double quote with preceding args.
#[test]
fn parse_unterminated_double_quote_after_args() {
    let result = parse_response_file_content("-O2 \"unterminated");
    assert_eq!(result, s(&["-O2", "unterminated"]));
}

/// Unterminated single quote with preceding args.
#[test]
fn parse_unterminated_single_quote_after_args() {
    let result = parse_response_file_content("-Wall 'never closed");
    assert_eq!(result, s(&["-Wall", "never closed"]));
}

/// Empty double quotes produce a single empty-string argument.
#[test]
fn parse_empty_double_quotes() {
    let result = parse_response_file_content("\"\"");
    assert_eq!(result, s(&[""]));
}

/// Empty single quotes produce a single empty-string argument.
#[test]
fn parse_empty_single_quotes() {
    let result = parse_response_file_content("''");
    assert_eq!(result, s(&[""]));
}

/// Adjacent empty quotes merge into one empty-string argument.
#[test]
fn parse_adjacent_empty_quotes() {
    // ""'' → one arg: ""
    let result = parse_response_file_content("\"\"''");
    assert_eq!(result, s(&[""]));
}

/// Mixed quote types in single argument: foo"bar"baz
#[test]
fn parse_mixed_quotes_in_single_arg() {
    let result = parse_response_file_content("foo\"bar\"baz");
    assert_eq!(result, s(&["foobarbaz"]));
}

/// Single-quote inside double-quote: "it's"
#[test]
fn parse_single_quote_inside_double() {
    let result = parse_response_file_content("\"it's a test\"");
    assert_eq!(result, s(&["it's a test"]));
}

/// Double-quote inside single-quote: 'say "hi"'
#[test]
fn parse_double_quote_inside_single() {
    let result = parse_response_file_content("'say \"hi\"'");
    assert_eq!(result, s(&["say \"hi\""]));
}

/// Quote at start, unquoted continuation: "foo"bar → foobar (one arg)
#[test]
fn parse_quoted_then_unquoted() {
    let result = parse_response_file_content("\"foo\"bar");
    assert_eq!(result, s(&["foobar"]));
}

/// Unquoted then quoted: foo"bar" → foobar (one arg)
#[test]
fn parse_unquoted_then_quoted() {
    let result = parse_response_file_content("foo\"bar\"");
    assert_eq!(result, s(&["foobar"]));
}

/// Adjacent double-quoted segments with no space: "abc""def" → abcdef
#[test]
fn parse_adjacent_double_quoted_segments() {
    let result = parse_response_file_content("\"abc\"\"def\"");
    assert_eq!(result, s(&["abcdef"]));
}

/// Alternating quote types: "a"'b'"c" → abc
#[test]
fn parse_alternating_quote_types() {
    let result = parse_response_file_content("\"a\"'b'\"c\"");
    assert_eq!(result, s(&["abc"]));
}

/// Nested-looking quotes (not truly nested): "a'b'c" → a'b'c
#[test]
fn parse_nested_looking_quotes() {
    let result = parse_response_file_content("\"a'b'c\"");
    assert_eq!(result, s(&["a'b'c"]));
}

/// Multiple empty-quoted args separated by space.
#[test]
fn parse_multiple_empty_quoted_args() {
    let result = parse_response_file_content("\"\" '' \"\"");
    assert_eq!(result, s(&["", "", ""]));
}

/// Quoted whitespace should be preserved.
#[test]
fn parse_quoted_whitespace_preserved() {
    let result = parse_response_file_content("\"  spaces  \" '\ttab\t'");
    assert_eq!(result, s(&["  spaces  ", "\ttab\t"]));
}

/// Only a quote char, nothing else.
#[test]
fn parse_lone_double_quote() {
    let result = parse_response_file_content("\"");
    assert_eq!(result, s(&[""]));
}

/// Only a single-quote char.
#[test]
fn parse_lone_single_quote() {
    let result = parse_response_file_content("'");
    assert_eq!(result, s(&[""]));
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 2: PARSER ADVERSARIAL — ESCAPE SEQUENCES
// ═══════════════════════════════════════════════════════════════════════════════

/// Backslash at end of double-quoted string (no char after backslash, quote not closed).
#[test]
fn parse_backslash_at_eof_in_double_quotes() {
    // "foo\ — backslash then EOF
    let result = parse_response_file_content("\"foo\\");
    assert_eq!(result, s(&["foo"]));
}

/// Backslash right before closing double quote: "foo\" — the quote is escaped!
/// This means the quote is consumed as literal, and we read to EOF.
#[test]
fn parse_backslash_before_closing_double_quote() {
    let result = parse_response_file_content("\"foo\\\"");
    // \" escapes the quote → foo" and then unterminated → "foo\""
    assert_eq!(result, s(&["foo\""]));
}

/// Double backslash before closing quote: "foo\\" — \\\\ → \, then " closes.
#[test]
fn parse_double_backslash_before_close() {
    let result = parse_response_file_content("\"foo\\\\\"");
    assert_eq!(result, s(&["foo\\"]));
}

/// Triple backslash before closing quote: "foo\\\" — \\\\ → \, then \" escapes quote.
#[test]
fn parse_triple_backslash_before_close() {
    let result = parse_response_file_content("\"foo\\\\\\\"");
    assert_eq!(result, s(&["foo\\\""]));
}

/// Many backslashes: even number before quote → string closes.
#[test]
fn parse_even_backslashes_before_close() {
    // "a\\\\" → a\\
    let result = parse_response_file_content("\"a\\\\\\\\\"");
    assert_eq!(result, s(&["a\\\\"]));
}

/// Unknown escape sequence in double quotes: \x → x (passthrough).
#[test]
fn parse_unknown_escape_passthrough() {
    let result = parse_response_file_content("\"\\x\\y\\z\"");
    assert_eq!(result, s(&["xyz"]));
}

/// Escape-n in double quotes: \n → newline.
#[test]
fn parse_escape_n_in_double_quotes() {
    let result = parse_response_file_content("\"line1\\nline2\"");
    assert_eq!(result, s(&["line1\nline2"]));
}

/// Escape-t in double quotes: \t → t (not tab! only \n, \\, \" are special).
#[test]
fn parse_escape_t_is_literal_t() {
    let result = parse_response_file_content("\"\\t\"");
    assert_eq!(result, s(&["t"]));
}

/// Backslash in single quotes: always literal.
#[test]
fn parse_backslash_in_single_quotes_literal() {
    let result = parse_response_file_content("'\\n\\t\\\\'");
    assert_eq!(result, s(&["\\n\\t\\\\"]));
}

/// Backslash in unquoted context: always literal.
#[test]
fn parse_backslash_unquoted_literal() {
    let result = parse_response_file_content("C:\\Users\\path");
    assert_eq!(result, s(&["C:\\Users\\path"]));
}

/// Backslash at very end of input (unquoted).
#[test]
fn parse_backslash_at_eof_unquoted() {
    let result = parse_response_file_content("path\\");
    assert_eq!(result, s(&["path\\"]));
}

/// Multiple escape sequences in a row.
#[test]
fn parse_chained_escapes() {
    let result = parse_response_file_content("\"\\\\\\n\\\"\"");
    // \\\\ → \, \\n → newline, \\" → "
    assert_eq!(result, s(&["\\\n\""]));
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 3: PARSER ADVERSARIAL — WHITESPACE HANDLING
// ═══════════════════════════════════════════════════════════════════════════════

/// Only spaces → empty.
#[test]
fn parse_only_spaces() {
    let result = parse_response_file_content("     ");
    assert!(result.is_empty());
}

/// Only tabs → empty.
#[test]
fn parse_only_tabs() {
    let result = parse_response_file_content("\t\t\t");
    assert!(result.is_empty());
}

/// Only newlines → empty.
#[test]
fn parse_only_newlines() {
    let result = parse_response_file_content("\n\n\n");
    assert!(result.is_empty());
}

/// Only carriage returns → empty.
#[test]
fn parse_only_carriage_returns() {
    let result = parse_response_file_content("\r\r\r");
    assert!(result.is_empty());
}

/// CRLF line endings between args.
#[test]
fn parse_crlf_between_args() {
    let result = parse_response_file_content("-O2\r\n-Wall\r\n-c\r\n");
    assert_eq!(result, s(&["-O2", "-Wall", "-c"]));
}

/// Mixed LF and CRLF.
#[test]
fn parse_mixed_lf_crlf() {
    let result = parse_response_file_content("-a\n-b\r\n-c\r-d");
    assert_eq!(result, s(&["-a", "-b", "-c", "-d"]));
}

/// Form feed and vertical tab are NOT whitespace separators (they're regular chars).
#[test]
fn parse_form_feed_vertical_tab_not_whitespace() {
    let result = parse_response_file_content("a\x0Cb\x0Bc");
    // \x0C = form feed, \x0B = vertical tab — not in the whitespace match
    assert_eq!(result, s(&["a\x0Cb\x0Bc"]));
}

/// Leading and trailing whitespace with single arg.
#[test]
fn parse_leading_trailing_whitespace_single_arg() {
    let result = parse_response_file_content("  \t\n  -O2  \t\n  ");
    assert_eq!(result, s(&["-O2"]));
}

/// Massive whitespace between args.
#[test]
fn parse_massive_whitespace_between_args() {
    let ws = " ".repeat(1000);
    let content = format!("-a{ws}-b{ws}-c");
    let result = parse_response_file_content(&content);
    assert_eq!(result, s(&["-a", "-b", "-c"]));
}

/// No whitespace at all → single argument.
#[test]
fn parse_no_whitespace_single_arg() {
    let result = parse_response_file_content("-DFOO=BAR");
    assert_eq!(result, s(&["-DFOO=BAR"]));
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 4: PARSER ADVERSARIAL — CONTENT EDGE CASES
// ═══════════════════════════════════════════════════════════════════════════════

/// UTF-8 BOM at start of content.
#[test]
fn parse_utf8_bom_at_start() {
    // BOM is U+FEFF, which is not whitespace in our parser → becomes part of first arg
    let content = "\u{FEFF}-O2 -Wall";
    let result = parse_response_file_content(content);
    assert_eq!(result.len(), 2);
    assert!(result[0].starts_with('\u{FEFF}'));
    assert_eq!(result[0], "\u{FEFF}-O2");
    assert_eq!(result[1], "-Wall");
}

/// Unicode multibyte characters in arguments.
#[test]
fn parse_unicode_multibyte() {
    let result = parse_response_file_content("-DMSG=\"\u{00E9}l\u{00E8}ve\" -I/\u{00FC}ber/path");
    assert_eq!(
        result,
        s(&["-DMSG=\u{00E9}l\u{00E8}ve", "-I/\u{00FC}ber/path"])
    );
}

/// Emoji in arguments.
#[test]
fn parse_emoji() {
    let result = parse_response_file_content("-DEMOJI=\"\u{1F600}\" file\u{1F4C4}.cpp");
    assert_eq!(result, s(&["-DEMOJI=\u{1F600}", "file\u{1F4C4}.cpp"]));
}

/// Null byte in content — should be treated as regular char.
#[test]
fn parse_null_byte() {
    let result = parse_response_file_content("-a\0b -c");
    assert_eq!(result.len(), 2);
    assert_eq!(result[0], "-a\0b");
    assert_eq!(result[1], "-c");
}

/// Content with @ signs (literal in parser, only meaningful in expand step).
#[test]
fn parse_at_signs_literal() {
    let result = parse_response_file_content("@file1 @@double -D@symbol");
    assert_eq!(result, s(&["@file1", "@@double", "-D@symbol"]));
}

/// Content that looks like shell syntax.
#[test]
fn parse_shell_syntax_literal() {
    let result = parse_response_file_content("-o output | rm -rf ; echo pwned");
    assert_eq!(
        result,
        s(&["-o", "output", "|", "rm", "-rf", ";", "echo", "pwned"])
    );
}

/// Arguments starting with `--`.
#[test]
fn parse_double_dash_args() {
    let result = parse_response_file_content("--target=x86_64 -- foo.cpp");
    assert_eq!(result, s(&["--target=x86_64", "--", "foo.cpp"]));
}

/// Single dash argument.
#[test]
fn parse_single_dash() {
    let result = parse_response_file_content("- -c foo.c");
    assert_eq!(result, s(&["-", "-c", "foo.c"]));
}

/// Arguments with equals signs.
#[test]
fn parse_equals_in_args() {
    let result =
        parse_response_file_content("-DFOO=BAR -DBAZ=\"q=1\" -std=c++17 --sysroot=/usr/local");
    assert_eq!(
        result,
        s(&[
            "-DFOO=BAR",
            "-DBAZ=q=1",
            "-std=c++17",
            "--sysroot=/usr/local"
        ])
    );
}

/// Very long single argument (10KB).
#[test]
fn parse_very_long_single_arg() {
    let long_arg = format!("-D{}", "A".repeat(10_000));
    let result = parse_response_file_content(&long_arg);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].len(), 10_002); // -D + 10000 A's
}

/// Many small arguments (10,000).
#[test]
fn parse_many_small_args() {
    let content: String = (0..10_000)
        .map(|i| format!("-D_{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    let result = parse_response_file_content(&content);
    assert_eq!(result.len(), 10_000);
    assert_eq!(result[0], "-D_0");
    assert_eq!(result[9999], "-D_9999");
}

/// Real-world GCC response file: Windows paths properly quoted.
/// Single quotes preserve backslashes; double quotes need `\\` for literal `\`.
#[test]
fn parse_realistic_windows_response_file() {
    // In real response files, paths with spaces MUST be quoted.
    // Single quotes preserve backslashes (important for Windows paths).
    // Double quotes require \\\\ for a literal backslash.
    let content = "'-IC:\\Program Files\\LLVM\\include'\n\
                   -IC:\\Users\\dev\\project\\src\n\
                   -DWIN32\n\
                   -D_WINDOWS\n\
                   -DCMAKE_INTDIR=\"Debug\"\n\
                   -D_DEBUG\n\
                   -std:c++17\n\
                   -Wall\n\
                   -Wextra\n\
                   -O0\n\
                   -g\n\
                   -c\n\
                   'C:\\Users\\dev\\project\\src\\main.cpp'\n\
                   -o 'C:\\Users\\dev\\project\\build\\main.obj'\n";
    let result = parse_response_file_content(content);
    // arg 0: single-quoted path (one token), then straightforward args
    assert_eq!(result[0], r"-IC:\Program Files\LLVM\include");
    assert_eq!(result[4], "-DCMAKE_INTDIR=Debug");
    assert_eq!(result[12], r"C:\Users\dev\project\src\main.cpp");
    assert_eq!(result[14], r"C:\Users\dev\project\build\main.obj");
}

/// Double quotes eat backslashes on Windows paths — a real trap.
#[test]
fn parse_double_quotes_eat_backslashes() {
    // "C:\foo\bar" → C:foobar (backslashes consumed as escape prefixes!)
    let result = parse_response_file_content("\"C:\\foo\\bar\"");
    // \f → f, \b → b (unknown escapes pass through the char, drop the \)
    assert_eq!(result, s(&["C:foobar"]));
}

/// To preserve backslashes in double quotes, double them.
#[test]
fn parse_doubled_backslashes_in_double_quotes() {
    let result = parse_response_file_content("\"C:\\\\foo\\\\bar\"");
    assert_eq!(result, s(&["C:\\foo\\bar"]));
}

/// Unquoted Windows path with spaces splits at the space (expected behavior).
#[test]
fn parse_unquoted_windows_path_with_space_splits() {
    let result = parse_response_file_content(r"-IC:\Program Files\LLVM\include");
    // Space is a separator — this produces TWO args, not one
    assert_eq!(result, s(&[r"-IC:\Program", r"Files\LLVM\include"]));
}

/// Real-world Clang response file: Linux-style with complex defines.
#[test]
fn parse_realistic_linux_response_file() {
    let content = "-I/usr/include\n\
                   -I/home/user/project/include\n\
                   -isystem /usr/include/c++/12\n\
                   -DVERSION=\"1.2.3\"\n\
                   -D'GREETING=hello world'\n\
                   -std=c++20\n\
                   -fPIC\n\
                   -Wall -Wextra -Werror\n\
                   -O2 -g\n\
                   -c /home/user/project/src/main.cpp\n\
                   -o /home/user/project/build/main.o\n";
    let result = parse_response_file_content(content);
    assert!(result.contains(&"-isystem".to_string()));
    assert!(result.contains(&"/usr/include/c++/12".to_string()));
    assert!(result.contains(&"-DVERSION=1.2.3".to_string()));
    assert!(result.contains(&"-DGREETING=hello world".to_string()));
}

/// Backslash-quote in unquoted context: `\` is literal, `"` starts quoted section.
/// This is a subtle parse trap — `\"` does NOT produce a literal quote in unquoted context.
#[test]
fn parse_backslash_quote_unquoted_context_trap() {
    // Input: -DBUILD_TYPE=\"Release\"
    // Parse: -DBUILD_TYPE=\ (literal backslash), then " starts quote,
    //        Release inside quote, then \" is escaped quote (still in quote),
    //        then EOF closes unclosed quote.
    let result = parse_response_file_content(r#"-DBUILD_TYPE=\"Release\""#);
    assert_eq!(result, s(&["-DBUILD_TYPE=\\Release\""]));
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 5: PARSER ADVERSARIAL — COMPLEX QUOTING INTERACTIONS
// ═══════════════════════════════════════════════════════════════════════════════

/// Double-quoted string containing newlines (literal, not escaped).
#[test]
fn parse_literal_newline_in_double_quotes() {
    let result = parse_response_file_content("\"line1\nline2\"");
    // Literal newline inside quotes — should be part of the arg
    assert_eq!(result, s(&["line1\nline2"]));
}

/// Single-quoted string containing newlines.
#[test]
fn parse_literal_newline_in_single_quotes() {
    let result = parse_response_file_content("'line1\nline2'");
    assert_eq!(result, s(&["line1\nline2"]));
}

/// Argument that is entirely quote chars: """" → two empty strings? Or ""?
/// Actually: " starts quote, " closes it → empty. Then " starts quote, " closes → empty.
/// Both adjacent → merge into one empty arg.
#[test]
fn parse_four_double_quotes() {
    let result = parse_response_file_content("\"\"\"\"");
    // "" → empty, "" → empty, adjacent → one empty arg
    assert_eq!(result, s(&[""]));
}

/// Complex: -DFOO='bar' "baz" qux → -DFOO=barbazqux (all one arg, no spaces)
#[test]
fn parse_complex_mixed_quoting() {
    let result = parse_response_file_content("-DFOO='bar'\"baz\"qux");
    assert_eq!(result, s(&["-DFOO=barbazqux"]));
}

/// Space inside quote then unquoted continuation: "a b"c → a bc
#[test]
fn parse_space_in_quote_then_unquoted() {
    let result = parse_response_file_content("\"a b\"c");
    assert_eq!(result, s(&["a bc"]));
}

/// Unquoted, then quoted with space: c"a b" → ca b
#[test]
fn parse_unquoted_then_quoted_with_space() {
    let result = parse_response_file_content("c\"a b\"");
    assert_eq!(result, s(&["ca b"]));
}

/// Three separate quoted segments forming one arg.
#[test]
fn parse_three_quoted_segments() {
    let result = parse_response_file_content("'A '\"B \"'C'");
    assert_eq!(result, s(&["A B C"]));
}

/// Escaped quote inside arg that transitions to unquoted.
#[test]
fn parse_escaped_quote_then_unquoted() {
    let result = parse_response_file_content("\"a\\\"b\" c");
    assert_eq!(result, s(&["a\"b", "c"]));
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 6: EXPANSION — DEPTH BOUNDARY TESTS
// ═══════════════════════════════════════════════════════════════════════════════

/// Helper: create a chain of N response files, each referencing the next.
/// The deepest file contains `-DLEAF`.
/// Returns the path to the outermost file.
fn create_nested_chain(dir: &Path, depth: usize) -> NormalizedPath {
    // Create files from deepest to shallowest
    let deepest = dir.join(format!("level_{depth}.rsp"));
    std::fs::write(&deepest, "-DLEAF").unwrap();

    let mut prev_path = NormalizedPath::new(deepest);
    for i in (0..depth).rev() {
        let this_path = dir.join(format!("level_{i}.rsp"));
        let content = format!("-DLEVEL_{i} @{}", prev_path.display());
        std::fs::write(&this_path, content).unwrap();
        prev_path = NormalizedPath::new(this_path);
    }
    prev_path
}

/// Exactly 10 levels of nesting (MAX_DEPTH=10). Should succeed.
/// Level 0 → level 1 → ... → level 9 → level 10 (leaf, no @ref).
/// depth parameter to expand_recursive goes: 0, 1, 2, ..., 9.
/// At depth 9, it reads level_9.rsp which has @level_10.rsp.
/// depth becomes 10, which is >= MAX_DEPTH → TooDeep.
/// So 10 levels of nesting actually fails! Let's verify the boundary.
#[test]
fn expand_depth_exactly_at_max() {
    let dir = tempfile::tempdir().unwrap();
    // 9 levels of nesting: level_0 → level_1 → ... → level_8 → level_9 (leaf)
    let root = create_nested_chain(dir.path(), 9);
    let args = s(&[&format!("@{}", root.display())]);
    let result = expand_response_files(&args);
    assert!(
        result.is_ok(),
        "9 levels of nesting should succeed: {result:?}"
    );
    let expanded = result.unwrap();
    assert!(expanded.contains(&"-DLEAF".to_string()));
    assert!(expanded.contains(&"-DLEVEL_0".to_string()));
}

/// 10 levels of nesting: should hit MAX_DEPTH and fail.
#[test]
fn expand_depth_one_past_max() {
    let dir = tempfile::tempdir().unwrap();
    let root = create_nested_chain(dir.path(), 10);
    let args = s(&[&format!("@{}", root.display())]);
    let result = expand_response_files(&args);
    assert!(result.is_err(), "10 levels should exceed MAX_DEPTH");
    assert!(
        matches!(result.unwrap_err(), ResponseFileError::TooDeep { .. }),
        "expected TooDeep error"
    );
}

/// Exactly MAX_DEPTH-1 levels: should succeed.
#[test]
fn expand_depth_one_below_max() {
    let dir = tempfile::tempdir().unwrap();
    let root = create_nested_chain(dir.path(), 8);
    let args = s(&[&format!("@{}", root.display())]);
    let result = expand_response_files(&args);
    assert!(result.is_ok(), "8 levels should succeed: {result:?}");
}

/// Wide fan at each level (each file has 3 @refs to children). Depth 3, fan 3 → 40 files.
#[test]
fn expand_wide_fan_nested() {
    let dir = tempfile::tempdir().unwrap();

    // Create leaf files
    for i in 0..9 {
        let path = dir.path().join(format!("leaf_{i}.rsp"));
        std::fs::write(&path, format!("-DLEAF_{i}")).unwrap();
    }

    // Create mid-level files, each referencing 3 leaves
    for i in 0..3 {
        let path = dir.path().join(format!("mid_{i}.rsp"));
        let mut content = format!("-DMID_{i}");
        for j in 0..3 {
            let leaf = dir.path().join(format!("leaf_{}.rsp", i * 3 + j));
            content.push_str(&format!(" @{}", leaf.display()));
        }
        std::fs::write(&path, content).unwrap();
    }

    // Create root referencing all mid files
    let root = dir.path().join("root.rsp");
    let mut content = "-DROOT".to_string();
    for i in 0..3 {
        let mid = dir.path().join(format!("mid_{i}.rsp"));
        content.push_str(&format!(" @{}", mid.display()));
    }
    std::fs::write(&root, content).unwrap();

    let args = s(&[&format!("@{}", root.display())]);
    let result = expand_response_files(&args).unwrap();

    // Should have: DROOT + 3×DMID_x + 9×DLEAF_x = 13 args
    assert_eq!(result.len(), 13);
    assert!(result.contains(&"-DROOT".to_string()));
    for i in 0..3 {
        assert!(result.contains(&format!("-DMID_{i}")));
    }
    for i in 0..9 {
        assert!(result.contains(&format!("-DLEAF_{i}")));
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 7: EXPANSION — CIRCULAR REFERENCE VARIANTS
// ═══════════════════════════════════════════════════════════════════════════════

/// Triangle cycle: A → B → C → A.
#[test]
fn expand_triangle_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let path_a = dir.path().join("a.rsp");
    let path_b = dir.path().join("b.rsp");
    let path_c = dir.path().join("c.rsp");

    std::fs::write(&path_a, format!("@{}", path_b.display())).unwrap();
    std::fs::write(&path_b, format!("@{}", path_c.display())).unwrap();
    std::fs::write(&path_c, format!("@{}", path_a.display())).unwrap();

    let args = s(&[&format!("@{}", path_a.display())]);
    let result = expand_response_files(&args);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        ResponseFileError::CircularReference { .. }
    ));
}

/// Cycle not involving root: A → B → C → D → B (cycle at B, not A).
#[test]
fn expand_deep_cycle_not_at_root() {
    let dir = tempfile::tempdir().unwrap();
    let path_a = dir.path().join("a.rsp");
    let path_b = dir.path().join("b.rsp");
    let path_c = dir.path().join("c.rsp");
    let path_d = dir.path().join("d.rsp");

    std::fs::write(&path_a, format!("@{}", path_b.display())).unwrap();
    std::fs::write(&path_b, format!("@{}", path_c.display())).unwrap();
    std::fs::write(&path_c, format!("@{}", path_d.display())).unwrap();
    std::fs::write(&path_d, format!("@{}", path_b.display())).unwrap();

    let args = s(&[&format!("@{}", path_a.display())]);
    let result = expand_response_files(&args);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        ResponseFileError::CircularReference { .. }
    ));
}

/// Diamond: A → {B, C}, B → D, C → D. NOT circular (D appears in siblings).
#[test]
fn expand_diamond_not_circular() {
    let dir = tempfile::tempdir().unwrap();
    let path_d = dir.path().join("d.rsp");
    let path_b = dir.path().join("b.rsp");
    let path_c = dir.path().join("c.rsp");
    let path_a = dir.path().join("a.rsp");

    std::fs::write(&path_d, "-DFROM_D").unwrap();
    std::fs::write(&path_b, format!("-DFROM_B @{}", path_d.display())).unwrap();
    std::fs::write(&path_c, format!("-DFROM_C @{}", path_d.display())).unwrap();
    std::fs::write(
        &path_a,
        format!("@{} @{}", path_b.display(), path_c.display()),
    )
    .unwrap();

    let args = s(&[&format!("@{}", path_a.display())]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["-DFROM_B", "-DFROM_D", "-DFROM_C", "-DFROM_D"]));
}

/// Self-reference with other content: file has args AND @self.
#[test]
fn expand_self_reference_with_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("self.rsp");
    std::fs::write(&path, format!("-O2 -Wall @{}", path.display())).unwrap();

    let args = s(&[&format!("@{}", path.display())]);
    let result = expand_response_files(&args);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        ResponseFileError::CircularReference { .. }
    ));
}

/// Same file referenced 3 times at top level (siblings) → should all expand.
#[test]
fn expand_same_file_three_siblings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shared.rsp");
    std::fs::write(&path, "-DSHARED").unwrap();

    let ref_str = format!("@{}", path.display());
    let args = s(&[&ref_str, &ref_str, &ref_str]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["-DSHARED", "-DSHARED", "-DSHARED"]));
}

/// A references B, then later references B again (sibling positions). Should work.
#[test]
fn expand_same_file_referenced_twice_in_parent() {
    let dir = tempfile::tempdir().unwrap();
    let path_b = dir.path().join("b.rsp");
    let path_a = dir.path().join("a.rsp");

    std::fs::write(&path_b, "-DFROM_B").unwrap();
    std::fs::write(
        &path_a,
        format!("@{} -DMIDDLE @{}", path_b.display(), path_b.display()),
    )
    .unwrap();

    let args = s(&[&format!("@{}", path_a.display())]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["-DFROM_B", "-DMIDDLE", "-DFROM_B"]));
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 8: EXPANSION — FILE CONTENT EDGE CASES
// ═══════════════════════════════════════════════════════════════════════════════

/// Empty response file → no args contributed.
#[test]
fn expand_empty_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.rsp");
    std::fs::write(&path, "").unwrap();

    let args = s(&["-O2", &format!("@{}", path.display()), "-Wall"]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["-O2", "-Wall"]));
}

/// Whitespace-only response file → no args contributed.
#[test]
fn expand_whitespace_only_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ws.rsp");
    std::fs::write(&path, "   \n\t\r\n  ").unwrap();

    let args = s(&["-O2", &format!("@{}", path.display())]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["-O2"]));
}

/// Response file with UTF-8 BOM.
#[test]
fn expand_file_with_bom() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bom.rsp");
    std::fs::write(&path, "\u{FEFF}-O2 -Wall").unwrap();

    let args = s(&[&format!("@{}", path.display())]);
    let result = expand_response_files(&args).unwrap();
    // BOM becomes part of first arg
    assert_eq!(result.len(), 2);
    assert!(result[0].ends_with("-O2"));
    assert_eq!(result[1], "-Wall");
}

/// Response file with CRLF line endings.
#[test]
fn expand_file_with_crlf() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crlf.rsp");
    std::fs::write(&path, "-O2\r\n-Wall\r\n-DNDEBUG\r\n").unwrap();

    let args = s(&[&format!("@{}", path.display())]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["-O2", "-Wall", "-DNDEBUG"]));
}

/// Response file that produces args starting with @.
/// These should be recursively expanded.
#[test]
fn expand_file_producing_at_args() {
    let dir = tempfile::tempdir().unwrap();
    let inner = dir.path().join("inner.rsp");
    std::fs::write(&inner, "-DINNER").unwrap();

    let outer = dir.path().join("outer.rsp");
    std::fs::write(&outer, format!("-DOUTER @{}", inner.display())).unwrap();

    let args = s(&[&format!("@{}", outer.display())]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["-DOUTER", "-DINNER"]));
}

/// Response file that contains a bare @ — should pass through.
#[test]
fn expand_file_with_bare_at() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bare_at.rsp");
    std::fs::write(&path, "-O2 @ -Wall").unwrap();

    let args = s(&[&format!("@{}", path.display())]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["-O2", "@", "-Wall"]));
}

/// Response file with double-@ prefix: @@file → @file is the filename.
#[test]
fn expand_double_at_prefix() {
    let dir = tempfile::tempdir().unwrap();
    // Create a file named "@oddname.rsp"
    let path = dir.path().join("@oddname.rsp");
    std::fs::write(&path, "-DODD").unwrap();

    // @@oddname.rsp → strip one @, look for file "@oddname.rsp"
    let args = s(&[&format!("@@{}", dir.path().join("oddname.rsp").display())]);
    // This will try to find file "@<dir>/oddname.rsp" which doesn't exist at that path
    // Actually, strip_prefix('@') gives "@<dir>/oddname.rsp"
    // so it looks for file named "@<dir>/oddname.rsp" which doesn't exist.
    // This should be a ReadError.
    let result = expand_response_files(&args);
    assert!(matches!(result, Err(ResponseFileError::ReadError { .. })));
}

/// Non-existent file gives ReadError.
#[test]
fn expand_nonexistent_file() {
    let args = s(&["@/this/path/surely/does/not/exist.rsp"]);
    let result = expand_response_files(&args);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        ResponseFileError::ReadError { .. }
    ));
}

/// Response file path with spaces.
#[test]
fn expand_path_with_spaces() {
    let dir = tempfile::tempdir().unwrap();
    let subdir = dir.path().join("path with spaces");
    std::fs::create_dir_all(&subdir).unwrap();
    let path = subdir.join("args.rsp");
    std::fs::write(&path, "-O2 -Wall").unwrap();

    let args = s(&[&format!("@{}", path.display())]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["-O2", "-Wall"]));
}

/// Mix of inline args and response file args — order preserved correctly.
#[test]
fn expand_interleaved_inline_and_file_args() {
    let dir = tempfile::tempdir().unwrap();
    let f1 = dir.path().join("f1.rsp");
    let f2 = dir.path().join("f2.rsp");
    std::fs::write(&f1, "-B -C").unwrap();
    std::fs::write(&f2, "-F -G").unwrap();

    let args = s(&[
        "-A",
        &format!("@{}", f1.display()),
        "-D",
        "-E",
        &format!("@{}", f2.display()),
        "-H",
    ]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["-A", "-B", "-C", "-D", "-E", "-F", "-G", "-H"]));
}

/// Large response file: 10,000 arguments.
#[test]
fn expand_large_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.rsp");
    let content: String = (0..10_000)
        .map(|i| format!("-D_{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, content).unwrap();

    let args = s(&[&format!("@{}", path.display())]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result.len(), 10_000);
    assert_eq!(result[0], "-D_0");
    assert_eq!(result[9999], "-D_9999");
}

/// Response file with quoted content containing the @ character.
#[test]
fn expand_file_with_quoted_at() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("quoted_at.rsp");
    // Quoted @file should NOT be expanded — it's an argument value.
    // But our parser doesn't distinguish: it just parses args.
    // "@nonexistent" becomes @nonexistent (without quotes) and WILL be expanded.
    // This tests that behavior.
    std::fs::write(&path, "-DFOO \"@not_a_file\"").unwrap();

    // "@not_a_file" after parsing becomes @not_a_file, which the expander
    // will try to treat as a response file reference.
    let args = s(&[&format!("@{}", path.display())]);
    let result = expand_response_files(&args);
    // Should fail because @not_a_file references a nonexistent file
    assert!(matches!(result, Err(ResponseFileError::ReadError { .. })));
}

/// Response file with all content on one line.
#[test]
fn expand_file_single_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oneline.rsp");
    std::fs::write(&path, "-c foo.cpp -o foo.o -O2 -Wall -DNDEBUG -std=c++17").unwrap();

    let args = s(&[&format!("@{}", path.display())]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(
        result,
        s(&[
            "-c",
            "foo.cpp",
            "-o",
            "foo.o",
            "-O2",
            "-Wall",
            "-DNDEBUG",
            "-std=c++17"
        ])
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 9: INTEGRATION — RESPONSE FILES + parse_invocation
// ═══════════════════════════════════════════════════════════════════════════════

/// All args come from a response file.
#[test]
fn integration_all_args_from_response_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("all.rsp");
    std::fs::write(&path, "-c foo.cpp -o foo.o -O2 -Wall").unwrap();

    let args = s(&[&format!("@{}", path.display())]);
    let expanded = expand_response_files(&args).unwrap();
    match zccache_compiler::parse_invocation("gcc", &expanded) {
        zccache_compiler::ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, Path::new("foo.cpp"));
            assert_eq!(c.output_file, Path::new("foo.o"));
            assert!(c.original_args.contains(&"-O2".to_string()));
            assert!(c.original_args.contains(&"-Wall".to_string()));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

/// -c flag comes from response file, source file is inline.
#[test]
fn integration_c_flag_from_response_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("flags.rsp");
    std::fs::write(&path, "-c -O2 -Wall").unwrap();

    let args = s(&["foo.cpp", &format!("@{}", path.display()), "-o", "foo.o"]);
    let expanded = expand_response_files(&args).unwrap();
    match zccache_compiler::parse_invocation("clang", &expanded) {
        zccache_compiler::ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, Path::new("foo.cpp"));
            assert_eq!(c.output_file, Path::new("foo.o"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

/// Non-cacheable flag (-E) comes from response file.
#[test]
fn integration_noncacheable_flag_from_response_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("preprocess.rsp");
    std::fs::write(&path, "-E -DNDEBUG").unwrap();

    let args = s(&["-c", "foo.cpp", &format!("@{}", path.display())]);
    let expanded = expand_response_files(&args).unwrap();
    match zccache_compiler::parse_invocation("gcc", &expanded) {
        zccache_compiler::ParsedInvocation::NonCacheable { .. } => { /* expected */ }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

/// Response file with quoted source path containing spaces.
#[test]
fn integration_quoted_source_path_from_response_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("quoted.rsp");
    std::fs::write(&path, "-c \"path with spaces/main.cpp\" -o main.o").unwrap();

    let args = s(&[&format!("@{}", path.display())]);
    let expanded = expand_response_files(&args).unwrap();
    match zccache_compiler::parse_invocation("gcc", &expanded) {
        zccache_compiler::ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, Path::new("path with spaces/main.cpp"));
            assert_eq!(c.output_file, Path::new("main.o"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

/// Nested response files that together form a cacheable invocation.
#[test]
fn integration_nested_response_files_cacheable() {
    let dir = tempfile::tempdir().unwrap();

    let flags_file = dir.path().join("flags.rsp");
    std::fs::write(&flags_file, "-O2 -Wall -DNDEBUG").unwrap();

    let outer_file = dir.path().join("outer.rsp");
    std::fs::write(
        &outer_file,
        format!("-c main.cpp @{}", flags_file.display()),
    )
    .unwrap();

    let args = s(&[&format!("@{}", outer_file.display()), "-o", "main.o"]);
    let expanded = expand_response_files(&args).unwrap();
    match zccache_compiler::parse_invocation("g++", &expanded) {
        zccache_compiler::ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, Path::new("main.cpp"));
            assert_eq!(c.output_file, Path::new("main.o"));
            assert!(c.original_args.contains(&"-O2".to_string()));
            assert!(c.original_args.contains(&"-Wall".to_string()));
            assert!(c.original_args.contains(&"-DNDEBUG".to_string()));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

/// Multiple source files spread across inline and response file → non-cacheable.
#[test]
fn integration_multiple_sources_via_response_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sources.rsp");
    std::fs::write(&path, "b.cpp").unwrap();

    let args = s(&["-c", "a.cpp", &format!("@{}", path.display())]);
    let expanded = expand_response_files(&args).unwrap();
    match zccache_compiler::parse_invocation("gcc", &expanded) {
        zccache_compiler::ParsedInvocation::MultiFile { compilations, .. } => {
            assert_eq!(compilations.len(), 2);
        }
        other => {
            panic!("expected MultiFile with 2 sources, got: {other:?}")
        }
    }
}

/// Response file with -D flag using = with quoted value, verify cache key capture.
#[test]
fn integration_define_with_quoted_value_in_cache_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("defines.rsp");
    std::fs::write(
        &path,
        "-DVERSION=\"1.2.3\" -DBUILD_TYPE=\"Release\" -DPATH=\"/usr/local\"",
    )
    .unwrap();

    let args = s(&["-c", "main.cpp", &format!("@{}", path.display())]);
    let expanded = expand_response_files(&args).unwrap();
    match zccache_compiler::parse_invocation("clang++", &expanded) {
        zccache_compiler::ParsedInvocation::Cacheable(c) => {
            assert!(c.original_args.contains(&"-DVERSION=1.2.3".to_string()));
            assert!(c
                .original_args
                .contains(&"-DBUILD_TYPE=Release".to_string()));
            assert!(c.original_args.contains(&"-DPATH=/usr/local".to_string()));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 10: STRESS — PERFORMANCE AND SCALE
// ═══════════════════════════════════════════════════════════════════════════════

/// Parse a 1MB response file content string.
#[test]
fn stress_parse_1mb_content() {
    // 1MB of -DFOO_XXXXX args
    let mut content = String::with_capacity(1_100_000);
    let mut count = 0;
    while content.len() < 1_000_000 {
        content.push_str(&format!("-DFOO_{count} "));
        count += 1;
    }
    let result = parse_response_file_content(&content);
    assert_eq!(result.len(), count);
}

/// Expand a chain where each file produces many args + one nested ref.
#[test]
fn stress_expand_chain_with_many_args() {
    let dir = tempfile::tempdir().unwrap();
    let depth = 5;

    let leaf = dir.path().join("leaf.rsp");
    let leaf_content: String = (0..100)
        .map(|i| format!("-DLEAF_{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    std::fs::write(&leaf, leaf_content).unwrap();

    let mut prev = leaf;
    for i in 0..depth {
        let this = dir.path().join(format!("level_{i}.rsp"));
        let args: String = (0..100)
            .map(|j| format!("-DL{i}_{j}"))
            .collect::<Vec<_>>()
            .join(" ");
        std::fs::write(&this, format!("{args} @{}", prev.display())).unwrap();
        prev = this;
    }

    let args = s(&[&format!("@{}", prev.display())]);
    let result = expand_response_files(&args).unwrap();
    // 5 levels × 100 args + 100 leaf args = 600
    assert_eq!(result.len(), (depth + 1) * 100);
}

/// Many sibling response file references (100 files).
#[test]
fn stress_expand_many_siblings() {
    let dir = tempfile::tempdir().unwrap();
    let mut arg_strs = Vec::new();

    for i in 0..100 {
        let path = dir.path().join(format!("sibling_{i}.rsp"));
        std::fs::write(&path, format!("-DSIB_{i}")).unwrap();
        arg_strs.push(format!("@{}", path.display()));
    }

    let args: Vec<String> = arg_strs;
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result.len(), 100);
    for (i, arg) in result.iter().enumerate() {
        assert_eq!(arg, &format!("-DSIB_{i}"));
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 11: PARSER ADVERSARIAL — REGRESSION GUARDS
// ═══════════════════════════════════════════════════════════════════════════════

/// Ensure that a response file arg is not confused with a compiler flag
/// when the arg starts with @.
#[test]
fn regression_at_arg_not_confused_with_flag() {
    // After expansion, @file becomes its contents. But if expansion produces
    // an arg like "@something" that isn't a file, expansion should error.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("produces_at.rsp");
    // This file's content, after parsing, includes "@nonexistent"
    std::fs::write(&path, "-O2 @nonexistent -Wall").unwrap();

    let args = s(&[&format!("@{}", path.display())]);
    let result = expand_response_files(&args);
    // @nonexistent should be treated as a response file reference → ReadError
    assert!(matches!(result, Err(ResponseFileError::ReadError { .. })));
}

/// Windows-style path in @reference.
#[test]
fn regression_windows_path_in_at_reference() {
    // On Windows, @C:\path\to\file.rsp should work.
    // On Unix, this path just won't exist, giving ReadError.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("win.rsp");
    std::fs::write(&path, "-O2").unwrap();

    // Use the actual temp path (works on both platforms)
    let args = s(&[&format!("@{}", path.display())]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["-O2"]));
}

/// Relative path ../ in response file reference.
#[test]
fn regression_relative_path_in_reference() {
    let dir = tempfile::tempdir().unwrap();
    let subdir = dir.path().join("sub");
    std::fs::create_dir_all(&subdir).unwrap();

    // Create file in parent dir
    let parent_file = dir.path().join("parent.rsp");
    std::fs::write(&parent_file, "-DFROM_PARENT").unwrap();

    // Create file in subdir that references ../parent.rsp
    let child_file = subdir.join("child.rsp");
    std::fs::write(
        &child_file,
        format!("-DFROM_CHILD @{}", parent_file.display()),
    )
    .unwrap();

    let args = s(&[&format!("@{}", child_file.display())]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["-DFROM_CHILD", "-DFROM_PARENT"]));
}

/// Verify that the `seen` set uses canonical paths, so symlinks are detected.
/// (Only works on Unix, so we skip on Windows.)
#[cfg(unix)]
#[test]
fn regression_symlink_cycle_detected() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let real_a = dir.path().join("real_a.rsp");
    let link_b = dir.path().join("link_b.rsp");

    // real_a references link_b, which is a symlink back to real_a
    std::fs::write(&real_a, format!("@{}", link_b.display())).unwrap();
    symlink(&real_a, &link_b).unwrap();

    let args = s(&[&format!("@{}", real_a.display())]);
    let result = expand_response_files(&args);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        ResponseFileError::CircularReference { .. }
    ));
}

/// Response file containing only comments-looking lines (# prefix).
/// There is no comment syntax in GCC response files — # is literal.
#[test]
fn regression_hash_is_not_comment() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hash.rsp");
    std::fs::write(&path, "# this is not a comment\n-O2").unwrap();

    let args = s(&[&format!("@{}", path.display())]);
    let result = expand_response_files(&args).unwrap();
    // # and "this", "is", etc. are all separate arguments
    assert!(result.contains(&"#".to_string()));
    assert!(result.contains(&"-O2".to_string()));
}

/// Response file with trailing newline vs without — should produce same args.
#[test]
fn regression_trailing_newline_irrelevant() {
    let dir = tempfile::tempdir().unwrap();

    let with_nl = dir.path().join("with_nl.rsp");
    std::fs::write(&with_nl, "-O2 -Wall\n").unwrap();

    let without_nl = dir.path().join("without_nl.rsp");
    std::fs::write(&without_nl, "-O2 -Wall").unwrap();

    let r1 = expand_response_files(&s(&[&format!("@{}", with_nl.display())])).unwrap();
    let r2 = expand_response_files(&s(&[&format!("@{}", without_nl.display())])).unwrap();
    assert_eq!(r1, r2);
}

/// Argument that is just "@" followed by space and then a real @file.
#[test]
fn regression_bare_at_then_real_at() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("real.rsp");
    std::fs::write(&path, "-DREAL").unwrap();

    let args = s(&["@", &format!("@{}", path.display())]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["@", "-DREAL"]));
}

#[cfg(windows)]
#[test]
fn windows_fbuild_shape_roundtrips_through_spill_rsp() {
    let dir = tempfile::tempdir().unwrap();
    let includes = dir.path().join("includes.rsp");
    std::fs::write(
        &includes,
        "'-IC:\\SDK\\Include'\n'-IC:\\Project Root\\Generated Headers'\n",
    )
    .unwrap();

    let args = force_spill_args_owned(vec![
        "-c".to_string(),
        r"C:\Project Root\src\main.c".to_string(),
        "-o".to_string(),
        r"C:\Project Root\build\main.o".to_string(),
        r#"-DVERSION="1.2.3""#.to_string(),
        r#"-DPKG_PATH="C:\Program Files\Vendor SDK\include""#.to_string(),
        format!("@{}", includes.display()),
    ]);

    let rsp = write_response_file_if_needed(&args, dir.path())
        .unwrap()
        .expect("spill rsp should be written");
    let written = std::fs::read_to_string(&rsp.path).unwrap();
    let reparsed = parse_response_file_content(&written);

    assert_eq!(reparsed, args);
}

#[cfg(windows)]
#[test]
fn windows_fbuild_shape_preserves_expanded_argv_semantics() {
    let original = s(&[
        "-c",
        r"C:\Project Root\src\main.c",
        "-o",
        r"C:\Project Root\build\main.o",
        r#"-DVERSION="1.2.3""#,
        r#"-DPKG_PATH="C:\Program Files\Vendor SDK\include""#,
        r"-IC:\SDK\Include",
        r"-IC:\Project Root\Generated Headers",
    ]);

    let dir = tempfile::tempdir().unwrap();
    let args = force_spill_args_owned(original.clone());
    let rsp = write_response_file_if_needed(&args, dir.path())
        .unwrap()
        .expect("spill rsp should be written");
    let written = std::fs::read_to_string(&rsp.path).unwrap();
    let reparsed = parse_response_file_content(&written);

    assert_eq!(reparsed[..original.len()], original);
}

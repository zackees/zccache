//! Adversarial parser tests for response file content tokenization.
//!
//! Targets parser dark corners (no filesystem expansion):
//! - Quoting edge cases (unterminated, mixed, nested-looking, adjacent)
//! - Escape sequences (boundary, unknown, chained, EOF)
//! - Whitespace (exotic, mixed line endings, BOM)
//! - Content edge cases (Unicode, null bytes, very long args)
//! - Complex quoting interactions
//!
//! Run all:    soldr cargo test -p zccache --test compiler_response_file_parser_test -- --nocapture
//! Run single: soldr cargo test -p zccache --test compiler_response_file_parser_test -- <test_name> --nocapture

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use zccache::compiler::response_file::parse_response_file_content;

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
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

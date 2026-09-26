//! Single-pass `#include` lexer (zccache#1670).
//!
//! Walks the source bytes once with no whole-file allocation. Backslash
//! line splices are skipped in place, and comments are tracked with a
//! small state machine, so only lines whose first significant byte is `#`
//! followed by `i` (or a comment/splice) are copied out and parsed.
//!
//! Comments are removed before a directive is parsed, and a leading UTF-8
//! byte-order mark is skipped. Known gaps, all shared with the previous
//! scanner (a missed include can serve a stale cache hit, so these matter
//! only on the static-scan fallback path; depfiles are authoritative):
//! - string literals are not tracked, so `"/*"` in code opens a comment;
//! - `//` or `/*` inside a header name (`"a//b.h"`) is read as a comment;
//! - a directive does not continue past a comment that spans lines.

use super::{parse_include_from_line, IncludeDirective};

/// Cursor over the source bytes that tracks the 1-based physical line.
struct Lexer<'a> {
    bytes: &'a [u8],
    pos: usize,
    line: u32,
}

impl Lexer<'_> {
    /// Length of the line splice (`\` + newline) at `at`, or 0.
    #[inline]
    fn splice_len(&self, at: usize) -> usize {
        if self.bytes.get(at) != Some(&b'\\') {
            return 0;
        }
        match self.bytes.get(at + 1) {
            Some(b'\n') => 2,
            Some(b'\r') if self.bytes.get(at + 2) == Some(&b'\n') => 3,
            Some(b'\r') => 2,
            _ => 0,
        }
    }

    /// Skip every line splice at `at`, returning the next real byte index.
    #[inline]
    fn skip_splices_from(&self, mut at: usize) -> usize {
        loop {
            let len = self.splice_len(at);
            if len == 0 {
                return at;
            }
            at += len;
        }
    }

    /// The current byte after any splices, which are consumed.
    #[inline]
    fn peek(&mut self) -> Option<u8> {
        loop {
            let len = self.splice_len(self.pos);
            if len == 0 {
                return self.bytes.get(self.pos).copied();
            }
            if self.bytes[self.pos + len - 1] == b'\n' {
                self.line += 1;
            }
            self.pos += len;
        }
    }

    /// The byte after the current one, looking through splices.
    #[inline]
    fn peek_next(&self) -> Option<u8> {
        let at = self.skip_splices_from(self.pos + 1);
        self.bytes.get(at).copied()
    }

    /// Consume the current byte and the next one (through any splices).
    #[inline]
    fn bump_pair(&mut self) {
        self.pos += 1;
        self.peek();
        self.pos += 1;
    }

    /// Move to the next occurrence of any of three bytes (or to the end).
    #[inline]
    fn seek3(&mut self, a: u8, b: u8, c: u8) -> Option<u8> {
        match memchr::memchr3(a, b, c, &self.bytes[self.pos..]) {
            Some(offset) => {
                self.pos += offset;
                Some(self.bytes[self.pos])
            }
            None => {
                self.pos = self.bytes.len();
                None
            }
        }
    }

    /// Consume the backslash at the cursor, with its newline if it splices.
    #[inline]
    fn skip_backslash(&mut self) {
        match self.splice_len(self.pos) {
            0 => self.pos += 1,
            len => {
                if self.bytes[self.pos + len - 1] == b'\n' {
                    self.line += 1;
                }
                self.pos += len;
            }
        }
    }

    /// Consume a block-comment body up to and including `*/`.
    ///
    /// Stops before a newline and returns `false` if the comment is still
    /// open there (or at end of input).
    fn skip_block_comment(&mut self) -> bool {
        while let Some(byte) = self.seek3(b'\n', b'*', b'\\') {
            match byte {
                b'\n' => return false,
                b'\\' => self.skip_backslash(),
                _ if self.peek_next() == Some(b'/') => {
                    self.bump_pair();
                    return true;
                }
                _ => self.pos += 1,
            }
        }
        false
    }

    /// Consume the rest of a logical line, stopping before its newline.
    ///
    /// Returns whether a block comment is still open at the newline.
    fn skip_rest_of_line(&mut self) -> bool {
        while let Some(byte) = self.seek3(b'\n', b'/', b'\\') {
            match byte {
                b'\n' => return false,
                b'\\' => self.skip_backslash(),
                _ => match self.peek_next() {
                    Some(b'/') => return self.skip_line_comment(),
                    Some(b'*') => {
                        self.bump_pair();
                        if !self.skip_block_comment() {
                            return true;
                        }
                    }
                    _ => self.pos += 1,
                },
            }
        }
        false
    }

    /// Consume a `//` comment through the end of its logical line.
    fn skip_line_comment(&mut self) -> bool {
        while let Some(byte) = self.seek3(b'\n', b'\\', b'\\') {
            if byte == b'\n' {
                break;
            }
            self.skip_backslash();
        }
        false
    }

    /// Copy the rest of a directive line into `out` without comments or
    /// splices, stopping before its newline.
    ///
    /// Returns whether a block comment is still open at the newline.
    fn collect_directive(&mut self, out: &mut Vec<u8>) -> bool {
        while let Some(byte) = self.peek() {
            match byte {
                b'\n' => return false,
                b'/' => match self.peek_next() {
                    Some(b'/') => return self.skip_line_comment(),
                    Some(b'*') => {
                        self.bump_pair();
                        if !self.skip_block_comment() {
                            return true;
                        }
                    }
                    _ => {
                        out.push(byte);
                        self.pos += 1;
                    }
                },
                _ => {
                    out.push(byte);
                    self.pos += 1;
                }
            }
        }
        false
    }
}

/// How a logical line starts once leading whitespace and comments are gone.
enum LineStart {
    /// `#` followed by something that may name `include`/`include_next`.
    IncludeCandidate,
    /// Any other directive or code; may still open a block comment.
    Other,
    /// The line ended (or input ended) with nothing significant on it.
    Empty,
}

/// Scan `source` for `#include` / `#include_next` directives in one pass.
pub(super) fn scan_directives(source: &str) -> Vec<IncludeDirective> {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let mut lexer = Lexer {
        bytes: source.as_bytes(),
        pos: 0,
        line: 1,
    };
    let mut results = Vec::new();
    let mut directive = Vec::new();
    let mut in_block_comment = false;

    while lexer.pos < lexer.bytes.len() {
        let start_line = lexer.line;
        let start = line_start(&mut lexer, &mut in_block_comment);
        match start {
            LineStart::IncludeCandidate => {
                directive.clear();
                in_block_comment = lexer.collect_directive(&mut directive);
                // Only ASCII sequences were removed from valid UTF-8, so this
                // cannot fail; a failure would just skip the line.
                if let Ok(text) = std::str::from_utf8(&directive) {
                    if let Some(dir) = parse_include_from_line(text) {
                        results.push(IncludeDirective {
                            line: start_line,
                            ..dir
                        });
                    }
                }
            }
            LineStart::Other => in_block_comment = lexer.skip_rest_of_line(),
            LineStart::Empty => {}
        }
        // Consume the newline (if any) that ended the logical line.
        if lexer.peek() == Some(b'\n') {
            lexer.pos += 1;
            lexer.line += 1;
        }
    }

    results
}

/// Skip leading whitespace and comments and classify the line.
///
/// Leaves the cursor on the first significant byte, or before the newline.
fn line_start(lexer: &mut Lexer<'_>, in_block_comment: &mut bool) -> LineStart {
    loop {
        if *in_block_comment {
            if !lexer.skip_block_comment() {
                return LineStart::Empty;
            }
            *in_block_comment = false;
        }
        match lexer.peek() {
            None | Some(b'\n') => return LineStart::Empty,
            Some(b' ' | b'\t' | b'\r' | b'\x0b' | b'\x0c') => lexer.pos += 1,
            Some(b'/') if lexer.peek_next() == Some(b'*') => {
                lexer.bump_pair();
                *in_block_comment = true;
            }
            Some(b'#') => {
                return if hash_may_start_include(lexer) {
                    LineStart::IncludeCandidate
                } else {
                    LineStart::Other
                };
            }
            Some(_) => return LineStart::Other,
        }
    }
}

/// Cheap filter on the bytes after `#`: an `include` directive name starts
/// with `i`, possibly after blanks, a comment, or a splice. Anything else
/// (`#define`, `#endif`, ...) cannot be an include.
fn hash_may_start_include(lexer: &Lexer<'_>) -> bool {
    let bytes = lexer.bytes;
    let mut at = lexer.pos + 1;
    while let Some(&byte) = bytes.get(at) {
        match byte {
            b' ' | b'\t' | b'\x0b' | b'\x0c' => at += 1,
            b'i' | b'/' | b'\\' => return true,
            _ => return false,
        }
    }
    false
}

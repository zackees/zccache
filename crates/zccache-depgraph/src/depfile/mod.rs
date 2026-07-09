//! Parser for GNU make dependency files (`.d` files).
//!
//! GCC/Clang emit these with `-MD -MF`. The format is:
//!
//! ```text
//! target.o: source.c header1.h \
//!   /usr/include/stdio.h path\ with\ spaces/foo.h
//! ```
//!
//! The parser extracts dependency paths, resolves relative paths against
//! a working directory, excludes the source file itself, and deduplicates.

mod canonicalize;
mod error;
mod parse;
mod strategy;

#[cfg(test)]
mod tests;

pub use canonicalize::{canonicalize_path, strip_win_prefix};
pub use error::DepfileError;
pub use parse::{parse_depfile, parse_depfile_path};
pub use strategy::{prepare_depfile, user_depfile_destination, DepfileStrategy};

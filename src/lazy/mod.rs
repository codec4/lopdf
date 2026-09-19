//! Reading a PDF's structure without loading the whole file.
//!
//! [`Document::load`](crate::Document::load) reads the entire file into memory and parses every
//! object before returning. [`LazyDocument`] instead reads the cross-reference data and trailer
//! when it opens, and then reads each object from its [`RandomAccessSource`] only when asked, so
//! looking at the catalog, the page tree, or an outline touches a small part of a large file.
//!
//! The lazy reader shares the eager reader's parsers and follows the same rules, with these
//! differences:
//! - The `%PDF-` header must appear within the first [`HEADER_SEARCH_LEN`] bytes.
//! - A cross-reference section that cannot be parsed is an error; the eager reader's recovery by
//!   scanning the whole file for objects is not attempted.
//! - [`LoadOptions::filter`](crate::LoadOptions) is ignored.

mod document;
mod single_page;
mod source;
mod stored_stream;

pub use document::LazyDocument;
pub use source::{FileSource, RandomAccessSource};

/// How far into a source the lazy reader looks for the `%PDF-` header.
pub const HEADER_SEARCH_LEN: usize = 4096;

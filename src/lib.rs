#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![deny(clippy::all)]

pub mod content;
pub mod encryption;
pub mod filters;
pub mod xobject;
pub mod xref;

#[cfg(feature = "lazy-reader")]
pub mod lazy;

#[macro_use]
mod object;
mod document;
mod incremental_document;

mod bookmarks;
mod cmap_section;
mod common_data_structures;
mod creator;
mod datetime;
mod destinations;
mod document_outline;
mod encodings;
mod error;
mod outlines;
mod page_content;
mod page_geometry;
mod page_labels;
mod processor;
mod toc;
mod writer;

mod load_options;
mod object_stream;
mod parser;
mod parser_aux;
mod reader;
mod resolver;
mod save_options;
mod stored_bytes;

#[cfg(feature = "font_embedding")]
mod font;

pub use document::Document;
pub use object::{Dictionary, Object, ObjectId, Stream, StringFormat};

pub use bookmarks::Bookmark;
pub use common_data_structures::{decode_text_string, text_string};
pub use destinations::Destination;
pub use document_outline::{DestinationView, DocumentOutline, OutlineItem, OutlineLimits, OutlineTarget};
pub use encodings::{Encoding, encode_utf8, encode_utf16_be};
pub use encryption::{EncryptionState, EncryptionVersion, Permissions};
pub use error::{DecompressError, Error, ParseError, Result};
pub use incremental_document::IncrementalDocument;
pub use load_options::{FilterFunc, LoadOptions};
pub use object_stream::{ObjectStream, ObjectStreamBuilder, ObjectStreamConfig};
pub use outlines::Outline;
pub use page_content::DecodeLimits;
pub use page_geometry::{PageGeometry, PageRect};
pub use page_labels::PageLabels;
pub use reader::{PdfMetadata, Reader};
pub use resolver::ObjectResolver;
pub use save_options::{SaveOptions, SaveOptionsBuilder};
pub use toc::{Toc, TocType};

pub use parser_aux::substr;
pub use parser_aux::substring;

#[cfg(feature = "font_embedding")]
pub use font::FontData;

//! Streams whose content stays in the source until it is read, a chunk at a time.
//!
//! [`LazyDocument::get_object`] reads a stream with its whole stored content. Page content is
//! only ever read front to back, so [`LazyDocument`] also reads a content stream's dictionary
//! alone, and then its content from the source as text extraction consumes it, decrypting on the
//! way. Memory then depends on neither the stored nor the decoded size of a page's content.

use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;

use log::warn;

use super::LazyDocument;
use super::document::{DEREF_LIMIT, MIN_OBJECT_WINDOW};
use super::source::RandomAccessSource;
use crate::encryption::crypt_filters::CryptFilter;
use crate::encryption::stream_crypt_filter;
use crate::encryption::stream_decryption::StreamDecryption;
use crate::page_content::{ContentStreams, StoredContent, StoredStream};
use crate::parser::{self, ObjectHead};
use crate::resolver::skip_unless_io;
use crate::stored_bytes::{CHUNK, StoredBytes};
use crate::xref::XrefEntry;
use crate::{Dictionary, Object, ObjectId, Result};

/// Room after a stream's content for the end-of-line marker and `endstream`.
const STREAM_END_LEN: usize = 16;

/// Object `id` as far as page content needs it.
enum Peeked {
    /// A stream whose content is in the source from `start`, `len` bytes long.
    Stream { dict: Dictionary, start: usize, len: usize },
    /// Any other object, or a stream read whole, such as one whose `/Length` is wrong.
    Object(Object),
    /// An object that cannot be read.
    Missing,
}

impl<S: RandomAccessSource> LazyDocument<S> {
    /// The content streams of page `page_id`, found as [`crate::Document::get_page_contents`]
    /// finds them, without reading their content.
    pub(super) fn page_content_ids(&self, page_id: ObjectId) -> Result<Vec<ObjectId>> {
        let mut streams = Vec::new();
        let Some(page) = skip_unless_io(self.get_dictionary(page_id))? else {
            return Ok(streams);
        };
        let Ok(mut contents) = page.get(b"Contents").cloned() else {
            return Ok(streams);
        };
        let mut derefs = 0;
        loop {
            match contents {
                Object::Reference(id) => match self.peek(id)? {
                    Peeked::Stream { .. } | Peeked::Object(Object::Stream(_)) | Peeked::Missing => streams.push(id),
                    Peeked::Object(object) => {
                        derefs += 1;
                        if derefs < DEREF_LIMIT {
                            contents = object;
                            continue;
                        }
                    }
                },
                Object::Array(items) => streams.extend(items.iter().filter_map(|item| item.as_reference().ok())),
                _ => {}
            }
            break;
        }
        Ok(streams)
    }

    /// Object `id`, without the content of a stream whose `/Length` and `endstream` check out.
    fn peek(&self, id: ObjectId) -> Result<Peeked> {
        let whole = || -> Result<Peeked> {
            Ok(match skip_unless_io(self.resolve(id, &mut HashSet::new()))? {
                Some(object) => Peeked::Object(object),
                None => Peeked::Missing,
            })
        };
        if let Some(object) = self.cached(id) {
            return Ok(Peeked::Object(object));
        }
        // An object in an object stream is never a stream.
        let Some(&XrefEntry::Normal { offset, generation }) = self.reference_table().get(id.0) else {
            return whole();
        };
        let offset = offset as usize;
        if generation != id.1 || offset >= self.len {
            return whole();
        }
        let next_index = self.normal_offsets.partition_point(|&next| next <= offset);
        let end = self.object_end(offset, self.normal_offsets.get(next_index).copied());
        let mut window_end = offset.saturating_add(MIN_OBJECT_WINDOW).min(end).min(self.len);
        let (dict, start) = loop {
            let bytes = self.read(offset, window_end)?;
            match parser::indirect_object_head(&bytes, id) {
                ObjectHead::Stream(dict, content_offset) => break (dict, offset + content_offset),
                ObjectHead::Truncated if window_end < end.min(self.len) => {
                    window_end = offset
                        .saturating_add((window_end - offset).saturating_mul(4))
                        .min(end)
                        .min(self.len);
                }
                ObjectHead::NotStream | ObjectHead::Truncated => return whole(),
            }
        };
        let length = match dict.get(b"Length") {
            Ok(Object::Reference(length_id)) if *length_id != id => {
                skip_unless_io(self.get_object(*length_id))?.and_then(|length| length.as_i64().ok())
            }
            Ok(length) => length.as_i64().ok(),
            Err(_) => None,
        };
        let Some(len) = length.and_then(|length| usize::try_from(length).ok()) else {
            return whole();
        };
        let Some(content_end) = start.checked_add(len).filter(|&content_end| content_end <= self.len) else {
            return whole();
        };
        let tail = self.read(content_end, content_end.saturating_add(STREAM_END_LEN).min(self.len))?;
        if !parser::is_stream_end(&tail) {
            return whole();
        }
        Ok(Peeked::Stream { dict, start, len })
    }

    /// How stream `id` with dictionary `dict` is decrypted, or `None` when it is stored in the
    /// clear, or cannot be decrypted and is then read as it is stored, as [`Self::get_object`]
    /// reads it.
    fn stream_decryption(&self, id: ObjectId, dict: &Dictionary, len: usize) -> Option<Decryption> {
        let state = self.encryption_state.as_ref()?;
        if Some(id) == self.encryption_dictionary {
            return None;
        }
        let filter = stream_crypt_filter(state, dict)?;
        let decryption = filter
            .compute_key(&state.file_encryption_key, id)
            .and_then(|key| Decryption::new(filter, key, len));
        match decryption {
            Ok(decryption) => Some(decryption),
            Err(error) => {
                warn!("stream {} {} did not decrypt: {error}", id.0, id.1);
                None
            }
        }
    }
}

impl<S: RandomAccessSource> ContentStreams for LazyDocument<S> {
    fn open(&self, id: ObjectId) -> Result<Option<StoredStream<'_>>> {
        Ok(match self.peek(id)? {
            Peeked::Stream { dict, start, len } => {
                let decryption = self.stream_decryption(id, &dict, len);
                Some(StoredStream {
                    dict: Cow::Owned(dict),
                    content: StoredContent::Read(Box::new(SourceBytes {
                        document: self,
                        start,
                        len,
                        position: 0,
                        decryption,
                    })),
                })
            }
            Peeked::Object(Object::Stream(stream)) => Some(StoredStream {
                dict: Cow::Owned(stream.dict),
                content: StoredContent::Held(Cow::Owned(stream.content)),
            }),
            Peeked::Object(_) | Peeked::Missing => None,
        })
    }
}

/// A stream's content read from the source a chunk at a time, and decrypted on the way.
struct SourceBytes<'a, S> {
    document: &'a LazyDocument<S>,
    start: usize,
    len: usize,
    position: usize,
    decryption: Option<Decryption>,
}

impl<S: RandomAccessSource> StoredBytes for SourceBytes<'_, S> {
    fn read_chunk(&mut self, output: &mut Vec<u8>) -> Result<bool> {
        if self.position == self.len {
            return match &mut self.decryption {
                Some(decryption) if !decryption.finished => {
                    decryption.finished = true;
                    decryption.state.finish(output)?;
                    Ok(true)
                }
                _ => Ok(false),
            };
        }
        let end = self.position.saturating_add(CHUNK).min(self.len);
        let bytes = self.document.read(self.start + self.position, self.start + end)?;
        self.position = end;
        match &mut self.decryption {
            Some(decryption) => decryption.state.update(&bytes, output)?,
            None => output.extend_from_slice(&bytes),
        }
        Ok(true)
    }

    fn rewind(&mut self) -> Result<()> {
        self.position = 0;
        if let Some(decryption) = &mut self.decryption {
            *decryption = Decryption::new(decryption.filter.clone(), decryption.key.clone(), self.len)?;
        }
        Ok(())
    }
}

struct Decryption {
    filter: Arc<dyn CryptFilter>,
    key: Vec<u8>,
    state: StreamDecryption,
    finished: bool,
}

impl Decryption {
    fn new(
        filter: Arc<dyn CryptFilter>, key: Vec<u8>, len: usize,
    ) -> std::result::Result<Self, crate::encryption::DecryptionError> {
        Ok(Self {
            state: StreamDecryption::new(filter.clone(), key.clone(), len)?,
            filter,
            key,
            finished: false,
        })
    }
}

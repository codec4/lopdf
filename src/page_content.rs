//! A page's content, decoded a chunk at a time.

use std::borrow::Cow;

use crate::filters::flate::Inflater;
use crate::object::filters_of;
use crate::stored_bytes::{CHUNK, SliceBytes, StoredBytes};
use crate::{DecompressError, Dictionary, Document, Error, Object, ObjectId, Result, Stream};

/// Bounds on what reading pages decodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeLimits {
    /// The most bytes that one stream held whole may decode to: a font's `/ToUnicode` CMap, a
    /// content stream that cannot be decoded a piece at a time, or one content operation. A stream
    /// read from a file is held whole only in those cases, and its stored bytes are bounded too.
    pub max_stream_size: usize,
    /// The most bytes that one page's content may decode to. Text extraction parses content as it
    /// decodes, so this and the total bound the work, not the memory.
    pub max_page_content_size: usize,
    /// The most bytes that the content of all the pages of one call may decode to together.
    pub max_total_content_size: usize,
}

impl DecodeLimits {
    /// No bounds at all.
    pub const UNBOUNDED: Self = Self::uniform(usize::MAX);

    /// `max` bytes for each stream held whole and for each page's content, and no bound across
    /// pages.
    pub const fn uniform(max: usize) -> Self {
        Self {
            max_stream_size: max,
            max_page_content_size: max,
            max_total_content_size: usize::MAX,
        }
    }
}

/// Where the content streams of a page come from: [`Document`] holds them, and the lazy reader
/// reads them from its source.
pub(crate) trait ContentStreams {
    /// Content stream `id`, or `None` when it is not a stream.
    fn open(&self, id: ObjectId) -> Result<Option<StoredStream<'_>>>;
}

/// A content stream: its dictionary, and its stored content.
pub(crate) struct StoredStream<'a> {
    pub(crate) dict: Cow<'a, Dictionary>,
    pub(crate) content: StoredContent<'a>,
}

pub(crate) enum StoredContent<'a> {
    /// Already in memory.
    Held(Cow<'a, [u8]>),
    /// Read from the source a chunk at a time.
    Read(Box<dyn StoredBytes + 'a>),
}

impl<'a> StoredContent<'a> {
    fn into_bytes(self) -> Box<dyn StoredBytes + 'a> {
        match self {
            Self::Held(bytes) => Box::new(SliceBytes::new(bytes)),
            Self::Read(bytes) => bytes,
        }
    }

    /// All of the content, or `None` when it is read from the source and is over `max` bytes.
    fn whole(self, max: usize) -> Result<Option<Cow<'a, [u8]>>> {
        let mut bytes = match self {
            Self::Held(bytes) => return Ok(Some(bytes)),
            Self::Read(bytes) => bytes,
        };
        let mut content = Vec::new();
        while bytes.read_chunk(&mut content)? {
            if content.len() > max {
                return Ok(None);
            }
        }
        Ok(Some(Cow::Owned(content)))
    }
}

impl ContentStreams for Document {
    fn open(&self, id: ObjectId) -> Result<Option<StoredStream<'_>>> {
        Ok(self
            .get_object(id)
            .and_then(Object::as_stream)
            .ok()
            .map(|stream| StoredStream {
                dict: Cow::Borrowed(&stream.dict),
                content: StoredContent::Held(Cow::Borrowed(&stream.content)),
            }))
    }
}

/// The content of a page: its content streams in order, each followed by a newline, decoded a
/// chunk at a time. `/FlateDecode` streams inflate as they are read; any other stream decodes
/// whole, within [`DecodeLimits::max_stream_size`], and one that cannot be decoded is read as it
/// is stored.
pub(crate) struct PageContent<'a> {
    streams: &'a dyn ContentStreams,
    ids: std::vec::IntoIter<ObjectId>,
    current: Option<Decoding<'a>>,
    len: usize,
    max_len: usize,
    /// The limit that an error past `max_len` reports: the page's, or the total's.
    limit: usize,
    max_stream_size: usize,
}

enum Decoding<'a> {
    Inflating(Inflater<Box<dyn StoredBytes + 'a>>),
    Decoded(Box<dyn StoredBytes + 'a>),
}

impl<'a> PageContent<'a> {
    /// The content in streams `ids`, after `spent` bytes of other pages' content.
    pub(crate) fn new(streams: &'a dyn ContentStreams, ids: Vec<ObjectId>, limits: DecodeLimits, spent: usize) -> Self {
        let remaining = limits.max_total_content_size.saturating_sub(spent);
        let (max_len, limit) = if limits.max_page_content_size <= remaining {
            (limits.max_page_content_size, limits.max_page_content_size)
        } else {
            (remaining, limits.max_total_content_size)
        };
        Self {
            streams,
            ids: ids.into_iter(),
            current: None,
            len: 0,
            max_len,
            limit,
            max_stream_size: limits.max_stream_size,
        }
    }

    /// How many bytes of content have been decoded.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Appends the next chunk of content to `output`, and returns false once there is none.
    pub(crate) fn read_into(&mut self, output: &mut Vec<u8>) -> Result<bool> {
        loop {
            let Some(decoding) = &mut self.current else {
                let Some(id) = self.ids.next() else {
                    return Ok(false);
                };
                if let Some(stream) = self.streams.open(id)? {
                    self.current = Some(self.start(stream)?);
                }
                continue;
            };
            let remaining = self.max_len.saturating_sub(self.len);
            let start = output.len();
            let more = match decoding {
                // One byte past the budget tells that the page goes over it.
                Decoding::Inflating(inflater) => {
                    inflater.read_into(output, CHUNK.min(remaining.saturating_add(1)))? > 0
                }
                Decoding::Decoded(bytes) => bytes.read_chunk(output)?,
            };
            let read = output.len() - start;
            if read > remaining {
                return Err(self.limit_exceeded());
            }
            self.len += read;
            if !more {
                output.push(b'\n');
                self.len += 1;
                self.current = None;
            }
            return Ok(true);
        }
    }

    fn start(&self, stream: StoredStream<'a>) -> Result<Decoding<'a>> {
        if Stream::inflates_alone(&stream.dict) {
            return Ok(Decoding::Inflating(Inflater::new(stream.content.into_bytes())));
        }
        // Without a filter, the stored bytes are the content.
        if filters_of(&stream.dict).is_err() {
            return Ok(Decoding::Decoded(stream.content.into_bytes()));
        }
        let Some(stored) = stream.content.whole(self.max_stream_size)? else {
            return Err(DecompressError::MemoryLimitExceeded {
                limit: self.max_stream_size,
            }
            .into());
        };
        let remaining = self.max_len.saturating_sub(self.len);
        let bound = remaining.min(self.max_stream_size);
        let bytes = match Stream::decode_stored(&stream.dict, &stored, Some(bound)) {
            Ok(bytes) => Cow::Owned(bytes),
            Err(Error::Decompress(DecompressError::MemoryLimitExceeded { .. })) if bound == remaining => {
                return Err(self.limit_exceeded());
            }
            Err(error @ Error::Decompress(DecompressError::MemoryLimitExceeded { .. })) => return Err(error),
            // Like `get_page_content`, a stream that cannot be decoded is read as it is stored.
            Err(_) => stored,
        };
        Ok(Decoding::Decoded(Box::new(SliceBytes::new(bytes))))
    }

    fn limit_exceeded(&self) -> Error {
        DecompressError::MemoryLimitExceeded { limit: self.limit }.into()
    }
}

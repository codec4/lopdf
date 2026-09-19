//! A page's content, decoded a chunk at a time.

use std::borrow::Cow;

use crate::filters::flate::Inflater;
use crate::{DecompressError, Document, Error, Object, ObjectId, Result, Stream};

/// The most content one [`PageContent::read_into`] appends.
const CHUNK: usize = 64 * 1024;

/// Bounds on what reading a page decodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeLimits {
    /// The most bytes that one stream held whole may decode to: a font's `/ToUnicode` CMap, a
    /// content stream that cannot be decoded a piece at a time, or one content operation.
    pub max_stream_size: usize,
    /// The most bytes that a page's content may decode to. Text extraction parses content as it
    /// decodes, so this bounds the work on a page rather than its memory.
    pub max_page_content_size: usize,
}

impl DecodeLimits {
    /// No bounds at all.
    pub const UNBOUNDED: Self = Self::uniform(usize::MAX);

    /// `max` bytes for both bounds.
    pub const fn uniform(max: usize) -> Self {
        Self {
            max_stream_size: max,
            max_page_content_size: max,
        }
    }
}

/// The content of a page: its content streams in order, each followed by a newline, decoded a
/// chunk at a time. `/FlateDecode` streams inflate as they are read; any other stream decodes
/// whole, within [`DecodeLimits::max_stream_size`], and one that cannot be decoded is read as it
/// is stored.
pub(crate) struct PageContent<'a> {
    document: &'a Document,
    streams: std::vec::IntoIter<ObjectId>,
    current: Option<Decoding<'a>>,
    len: usize,
    limits: DecodeLimits,
}

enum Decoding<'a> {
    Inflating(Inflater<'a>),
    Decoded { bytes: Cow<'a, [u8]>, position: usize },
}

impl<'a> PageContent<'a> {
    pub(crate) fn new(document: &'a Document, page_id: ObjectId, limits: DecodeLimits) -> Self {
        Self {
            document,
            streams: document.get_page_contents(page_id).into_iter(),
            current: None,
            len: 0,
            limits,
        }
    }

    /// Appends the next chunk of content to `output`, and returns false once there is none.
    pub(crate) fn read_into(&mut self, output: &mut Vec<u8>) -> Result<bool> {
        loop {
            let Some(decoding) = &mut self.current else {
                let Some(id) = self.streams.next() else {
                    return Ok(false);
                };
                if let Ok(stream) = self.document.get_object(id).and_then(Object::as_stream) {
                    self.current = Some(self.start(stream)?);
                }
                continue;
            };
            let remaining = self.limits.max_page_content_size.saturating_sub(self.len);
            // One byte past the budget tells that the page goes over it.
            let wanted = CHUNK.min(remaining.saturating_add(1));
            let read = match decoding {
                Decoding::Inflating(inflater) => inflater.read_into(output, wanted),
                Decoding::Decoded { bytes, position } => {
                    let chunk = &bytes[*position..];
                    let chunk = &chunk[..chunk.len().min(wanted)];
                    output.extend_from_slice(chunk);
                    *position += chunk.len();
                    chunk.len()
                }
            };
            if read > remaining {
                return Err(self.page_limit_exceeded());
            }
            self.len += read;
            if read == 0 {
                output.push(b'\n');
                self.len += 1;
                self.current = None;
            }
            return Ok(true);
        }
    }

    fn start(&self, stream: &'a Stream) -> Result<Decoding<'a>> {
        if stream.inflates_alone() {
            return Ok(Decoding::Inflating(Inflater::new(&stream.content)));
        }
        let stored = || Decoding::Decoded {
            bytes: Cow::Borrowed(&stream.content),
            position: 0,
        };
        // Without a filter, the stored bytes are the content.
        if stream.filters().is_err() {
            return Ok(stored());
        }
        let remaining = self.limits.max_page_content_size.saturating_sub(self.len);
        let bound = remaining.min(self.limits.max_stream_size);
        match stream.decompressed_content_with_limit(bound) {
            Ok(bytes) => Ok(Decoding::Decoded {
                bytes: Cow::Owned(bytes),
                position: 0,
            }),
            Err(Error::Decompress(DecompressError::MemoryLimitExceeded { .. })) if bound == remaining => {
                Err(self.page_limit_exceeded())
            }
            Err(error @ Error::Decompress(DecompressError::MemoryLimitExceeded { .. })) => Err(error),
            Err(_) => Ok(stored()),
        }
    }

    fn page_limit_exceeded(&self) -> Error {
        DecompressError::MemoryLimitExceeded {
            limit: self.limits.max_page_content_size,
        }
        .into()
    }
}

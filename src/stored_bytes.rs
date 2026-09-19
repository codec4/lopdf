//! A stream's stored bytes, read a chunk at a time.

use std::borrow::Cow;

use crate::Result;

/// The most stored bytes that one read gives.
pub(crate) const CHUNK: usize = 64 * 1024;

/// A stream's content as the file stores it, before any filter, read front to back a chunk at a
/// time, so that it need not be held whole.
pub(crate) trait StoredBytes {
    /// Appends the next chunk to `output`, and returns false once there is none. A chunk may be
    /// empty before the end.
    fn read_chunk(&mut self, output: &mut Vec<u8>) -> Result<bool>;

    /// Starts again from the first byte.
    fn rewind(&mut self) -> Result<()>;
}

impl<B: StoredBytes + ?Sized> StoredBytes for Box<B> {
    fn read_chunk(&mut self, output: &mut Vec<u8>) -> Result<bool> {
        (**self).read_chunk(output)
    }

    fn rewind(&mut self) -> Result<()> {
        (**self).rewind()
    }
}

/// Stored bytes held in memory.
pub(crate) struct SliceBytes<'a> {
    bytes: Cow<'a, [u8]>,
    position: usize,
}

impl<'a> SliceBytes<'a> {
    pub(crate) fn new(bytes: impl Into<Cow<'a, [u8]>>) -> Self {
        Self {
            bytes: bytes.into(),
            position: 0,
        }
    }
}

impl StoredBytes for SliceBytes<'_> {
    fn read_chunk(&mut self, output: &mut Vec<u8>) -> Result<bool> {
        let rest = &self.bytes[self.position..];
        if rest.is_empty() {
            return Ok(false);
        }
        let chunk = &rest[..rest.len().min(CHUNK)];
        output.extend_from_slice(chunk);
        self.position += chunk.len();
        Ok(true)
    }

    fn rewind(&mut self) -> Result<()> {
        self.position = 0;
        Ok(())
    }
}

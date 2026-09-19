//! `/FlateDecode`, inflated a piece at a time.

use flate2::{Decompress, FlushDecompress, Status};
use log::warn;

/// The most bytes one step of [`Inflater::read_into`] inflates.
const STEP: usize = 64 * 1024;

/// Inflates a `/FlateDecode` stream a piece at a time, so that its decoded bytes need not be held
/// at once.
///
/// Decoding is lenient. A stream that goes wrong keeps every byte before the fault, and a zlib
/// stream that yields nothing, such as one whose checksum encryption broke, is read again as raw
/// deflate after its two-byte header.
pub(crate) struct Inflater<'a> {
    input: &'a [u8],
    decompress: Decompress,
    produced: bool,
    raw: bool,
    done: bool,
}

impl<'a> Inflater<'a> {
    pub(crate) fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            decompress: Decompress::new(true),
            produced: false,
            raw: false,
            done: input.is_empty(),
        }
    }

    /// Appends up to `max_len` inflated bytes to `output`, and returns how many it appended, which
    /// is fewer only at the end of the stream.
    pub(crate) fn read_into(&mut self, output: &mut Vec<u8>, max_len: usize) -> usize {
        let start = output.len();
        while !self.done && output.len() - start < max_len {
            let len = output.len();
            output.resize(len + STEP.min(max_len - (len - start)), 0);
            let consumed = self.decompress.total_in() as usize;
            let before = self.decompress.total_out();
            let result = self
                .decompress
                .decompress(&self.input[consumed..], &mut output[len..], FlushDecompress::None);
            let written = (self.decompress.total_out() - before) as usize;
            output.truncate(len + written);
            self.produced |= written > 0;
            match result {
                Ok(Status::StreamEnd) => self.done = true,
                // Out of input before the end of the stream.
                Ok(_) if written == 0 && self.decompress.total_in() as usize == consumed => self.done = true,
                Ok(_) => {}
                Err(error) => {
                    warn!("{error}");
                    self.fall_back();
                }
            }
        }
        output.len() - start
    }

    fn fall_back(&mut self) {
        if !self.produced && !self.raw && self.input.len() > 2 {
            self.input = &self.input[2..];
            self.decompress = Decompress::new(false);
            self.raw = true;
        } else {
            self.done = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::{DeflateEncoder, ZlibEncoder};

    use super::*;

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn inflate(input: &[u8], step: usize) -> Vec<u8> {
        let mut inflater = Inflater::new(input);
        let mut output = Vec::new();
        while inflater.read_into(&mut output, step) == step {}
        output
    }

    fn text(len: usize) -> Vec<u8> {
        (0..len).map(|i| b"BT /F1 12 Tf (Hello) Tj ET\n"[i % 27]).collect()
    }

    #[test]
    fn reads_in_steps_of_any_size() {
        let data = text(300_000);
        let compressed = zlib(&data);
        for step in [1, 7, 4096, STEP, STEP + 1, usize::MAX] {
            assert_eq!(inflate(&compressed, step.min(400_000)), data, "step {step}");
        }
    }

    #[test]
    fn keeps_the_bytes_before_a_fault() {
        let data = text(300_000);
        let mut compressed = zlib(&data);
        let len = compressed.len();
        compressed.truncate(len / 2);
        compressed.extend_from_slice(&[0xff; 64]);

        let output = inflate(&compressed, usize::MAX);

        // What the bytes after the cut decode to before the fault shows depends on the backend.
        let kept = data.len() / 4;
        assert!(output.len() > kept, "{}", output.len());
        assert_eq!(output[..kept], data[..kept]);
    }

    #[test]
    fn keeps_everything_when_only_the_checksum_is_wrong() {
        let data = text(300_000);
        let mut compressed = zlib(&data);
        let len = compressed.len();
        for byte in &mut compressed[len - 4..] {
            *byte ^= 0xff;
        }
        assert_eq!(inflate(&compressed, usize::MAX), data);
    }

    #[test]
    fn reads_raw_deflate_after_a_header_when_zlib_yields_nothing() {
        let data = text(1000);
        let mut encoder = DeflateEncoder::new(vec![0x78, 0x00], Compression::default());
        encoder.write_all(&data).unwrap();
        let broken_header = encoder.finish().unwrap();
        assert_eq!(inflate(&broken_header, usize::MAX), data);
    }

    #[test]
    fn a_truncated_stream_ends_where_its_input_does() {
        let data = text(300_000);
        let compressed = zlib(&data);
        let output = inflate(&compressed[..compressed.len() / 2], 4096);
        assert!(!output.is_empty());
        assert_eq!(output, data[..output.len()]);
        assert!(inflate(&[], 4096).is_empty());
    }
}

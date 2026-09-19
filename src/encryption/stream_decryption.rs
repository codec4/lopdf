//! Stream content decrypted a piece at a time.

use std::sync::Arc;

use aes::cipher::block_padding::Padding;
use aes::cipher::{BlockModeDecrypt, KeyIvInit};
use log::warn;

use super::DecryptionError;
use super::crypt_filters::CryptFilter;
use super::pkcs5::Pkcs5;
use super::rc4::{Rc4, Rc4Keystream};

const BLOCK: usize = 16;

/// Decrypts a stream's content as it is read, giving what [`CryptFilter::decrypt`] gives for the
/// whole content. The one difference: AES padding that does not check out keeps the last block as
/// it decrypted, where whole decryption fails.
pub(crate) enum StreamDecryption {
    Identity,
    Rc4(Box<Rc4Keystream>),
    Aes(AesCbc),
    /// A filter this module does not know, which decrypts the whole content at the end.
    Whole {
        filter: Arc<dyn CryptFilter>,
        key: Vec<u8>,
        ciphertext: Vec<u8>,
    },
}

impl StreamDecryption {
    /// Decryption of `len` bytes of ciphertext with `filter` and the object's `key`. Fails where
    /// whole decryption fails before reading the content, such as for an AES ciphertext that is
    /// not whole blocks.
    pub(crate) fn new(filter: Arc<dyn CryptFilter>, key: Vec<u8>, len: usize) -> Result<Self, DecryptionError> {
        Ok(match filter.method() {
            b"Identity" => Self::Identity,
            b"V2" => Self::Rc4(Box::new(Rc4::new(&key).keystream())),
            method @ (b"AESV2" | b"AESV3") => {
                let key_len = if method == b"AESV2" { 16 } else { 32 };
                if key.len() != key_len {
                    return Err(DecryptionError::InvalidKeyLength);
                }
                if !len.is_multiple_of(BLOCK) {
                    return Err(DecryptionError::InvalidCipherTextLength);
                }
                Self::Aes(AesCbc {
                    key,
                    decryptor: None,
                    pending: Vec::new(),
                })
            }
            _ => Self::Whole {
                filter,
                key,
                ciphertext: Vec::new(),
            },
        })
    }

    /// Decrypts the next piece of ciphertext, and appends the plaintext that is ready to `output`.
    pub(crate) fn update(&mut self, ciphertext: &[u8], output: &mut Vec<u8>) -> Result<(), DecryptionError> {
        match self {
            Self::Identity => output.extend_from_slice(ciphertext),
            Self::Rc4(keystream) => {
                let start = output.len();
                output.extend_from_slice(ciphertext);
                keystream.apply(&mut output[start..]);
            }
            Self::Aes(aes) => aes.update(ciphertext, output)?,
            Self::Whole { ciphertext: all, .. } => all.extend_from_slice(ciphertext),
        }
        Ok(())
    }

    /// Appends the rest of the plaintext, once all the ciphertext has been given.
    pub(crate) fn finish(&mut self, output: &mut Vec<u8>) -> Result<(), DecryptionError> {
        match self {
            Self::Identity | Self::Rc4(_) => {}
            Self::Aes(aes) => aes.finish(output)?,
            Self::Whole {
                filter,
                key,
                ciphertext,
            } => {
                output.extend_from_slice(&filter.decrypt(key, ciphertext)?);
                ciphertext.clear();
            }
        }
        Ok(())
    }
}

/// AES in CBC mode, for 128-bit and 256-bit keys: the first block is the IV, and the last block
/// ends in PKCS#5 padding.
pub(crate) struct AesCbc {
    key: Vec<u8>,
    decryptor: Option<CbcDecryptor>,
    /// Ciphertext not decrypted yet: the IV until it is whole, then what does not fill a block,
    /// and always the last whole block, whose padding is checked at the end.
    pending: Vec<u8>,
}

enum CbcDecryptor {
    Aes128(Box<cbc::Decryptor<aes::Aes128>>),
    Aes256(Box<cbc::Decryptor<aes::Aes256>>),
}

impl AesCbc {
    fn update(&mut self, ciphertext: &[u8], output: &mut Vec<u8>) -> Result<(), DecryptionError> {
        self.pending.extend_from_slice(ciphertext);
        self.start()?;
        let Self { decryptor, pending, .. } = self;
        let Some(decryptor) = decryptor else {
            return Ok(());
        };
        let ready = pending.len().saturating_sub(1) / BLOCK * BLOCK;
        let start = output.len();
        output.extend_from_slice(&pending[..ready]);
        decryptor.decrypt(&mut output[start..]);
        pending.drain(..ready);
        Ok(())
    }

    fn finish(&mut self, output: &mut Vec<u8>) -> Result<(), DecryptionError> {
        self.start()?;
        let Self { decryptor, pending, .. } = self;
        // Without an IV the ciphertext was empty, and the IV alone decrypts to nothing, as in whole
        // decryption.
        let Some(decryptor) = decryptor.as_mut().filter(|_| !pending.is_empty()) else {
            return Ok(());
        };
        let mut block = std::mem::take(pending);
        decryptor.decrypt(&mut block);
        match Pkcs5::raw_unpad(&block) {
            Ok(plaintext) => output.extend_from_slice(plaintext),
            Err(_) => {
                warn!("AES padding does not check out; keeping the last block as it decrypted");
                output.extend_from_slice(&block);
            }
        }
        Ok(())
    }

    /// Sets up the decryptor once the IV has arrived.
    fn start(&mut self) -> Result<(), DecryptionError> {
        if self.decryptor.is_some() || self.pending.len() < BLOCK {
            return Ok(());
        }
        let iv: [u8; BLOCK] = self.pending[..BLOCK].try_into().expect("a whole block");
        self.decryptor = Some(match self.key.len() {
            16 => {
                let key: &[u8; 16] = self.key.as_slice().try_into().expect("a 128-bit key");
                CbcDecryptor::Aes128(Box::new(cbc::Decryptor::new(key.into(), &iv.into())))
            }
            32 => {
                let key: &[u8; 32] = self.key.as_slice().try_into().expect("a 256-bit key");
                CbcDecryptor::Aes256(Box::new(cbc::Decryptor::new(key.into(), &iv.into())))
            }
            _ => return Err(DecryptionError::InvalidKeyLength),
        });
        self.pending.drain(..BLOCK);
        Ok(())
    }
}

impl CbcDecryptor {
    /// Decrypts `data`, whole blocks, in place.
    fn decrypt(&mut self, data: &mut [u8]) {
        for block in data.as_chunks_mut::<BLOCK>().0 {
            match self {
                Self::Aes128(decryptor) => decryptor.decrypt_block(block.into()),
                Self::Aes256(decryptor) => decryptor.decrypt_block(block.into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encryption::crypt_filters::{Aes128CryptFilter, Aes256CryptFilter, IdentityCryptFilter, Rc4CryptFilter};

    fn decrypt_in_pieces(
        filter: Arc<dyn CryptFilter>, key: &[u8], ciphertext: &[u8], piece: usize,
    ) -> Result<Vec<u8>, DecryptionError> {
        let mut decryption = StreamDecryption::new(filter, key.to_vec(), ciphertext.len())?;
        let mut output = Vec::new();
        for chunk in ciphertext.chunks(piece.max(1)) {
            decryption.update(chunk, &mut output)?;
        }
        decryption.finish(&mut output)?;
        Ok(output)
    }

    #[test]
    fn every_filter_decrypts_in_pieces_as_it_does_whole() {
        let filters: [(Arc<dyn CryptFilter>, Vec<u8>); 4] = [
            (Arc::new(IdentityCryptFilter), vec![1; 16]),
            (Arc::new(Rc4CryptFilter), (1..=16).collect()),
            (Arc::new(Aes128CryptFilter), (1..=16).collect()),
            (Arc::new(Aes256CryptFilter), (1..=32).collect()),
        ];
        for (filter, key) in filters {
            for len in [0, 1, 15, 16, 17, 31, 32, 33, 1000, 4096] {
                let plaintext: Vec<u8> = (0..len).map(|i| (i * 7 % 251) as u8).collect();
                let ciphertext = filter.encrypt(&key, &plaintext).unwrap();
                let whole = filter.decrypt(&key, &ciphertext).unwrap();
                assert_eq!(whole, plaintext);
                for piece in [1, 5, 16, 17, 100, ciphertext.len()] {
                    let method = String::from_utf8_lossy(filter.method()).into_owned();
                    assert_eq!(
                        decrypt_in_pieces(filter.clone(), &key, &ciphertext, piece).unwrap(),
                        whole,
                        "{method}, {len} bytes in pieces of {piece}"
                    );
                }
            }
        }
    }

    #[test]
    fn aes_fails_as_whole_decryption_does_before_reading() {
        let filter: Arc<dyn CryptFilter> = Arc::new(Aes128CryptFilter);
        let key: Vec<u8> = (1..=16).collect();
        assert!(matches!(
            StreamDecryption::new(filter.clone(), key.clone(), 33),
            Err(DecryptionError::InvalidCipherTextLength)
        ));
        assert!(matches!(
            StreamDecryption::new(filter.clone(), key[..8].to_vec(), 32),
            Err(DecryptionError::InvalidKeyLength)
        ));
        assert!(decrypt_in_pieces(filter.clone(), &key, &[], 4).unwrap().is_empty());
        assert!(decrypt_in_pieces(filter, &key, &[9; 16], 4).unwrap().is_empty());
    }

    #[test]
    fn aes_padding_that_does_not_check_out_keeps_the_last_block() {
        let filter: Arc<dyn CryptFilter> = Arc::new(Aes128CryptFilter);
        let key: Vec<u8> = (1..=16).collect();
        let plaintext = vec![b'x'; 40];
        let mut ciphertext = filter.encrypt(&key, &plaintext).unwrap();
        let last = ciphertext.len() - 1;
        ciphertext[last] ^= 0xff;
        assert!(filter.decrypt(&key, &ciphertext).is_err());

        let output = decrypt_in_pieces(filter, &key, &ciphertext, 7).unwrap();

        assert_eq!(output.len(), 48);
        assert_eq!(output[..32], plaintext[..32]);
    }
}

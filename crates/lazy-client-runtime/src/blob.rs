//! Encrypted file contents, written and read a chunk at a time (slice 3A).
//!
//! The outbox (files waiting to be uploaded) and the image cache hold files of up to 25 MiB, so
//! they are never sealed in one piece in memory like the state files. A blob is a header
//! (`LZBL`, format, kind, 16 random bytes) followed by chunks of 64 KiB of plaintext, each sealed
//! with XChaCha20-Poly1305 under a nonce made of the header's random bytes and the chunk's number.
//! The header and a "last chunk" flag are authenticated with every chunk, so a chunk cannot be
//! moved, dropped, duplicated, or have the end cut off without the read failing.
//!
//! The key is derived from the profile's state key for blobs only, so a blob can never be
//! mistaken for (or substituted by) a state file.

use std::fs::File;
use std::io::{self, Read, Write};

use chacha20poly1305::XChaCha20Poly1305;
use chacha20poly1305::aead::{Aead, Key, KeyInit, Nonce, Payload};
use sha2::{Digest, Sha256};

const MAGIC: &[u8; 4] = b"LZBL";
const FORMAT: u16 = 1;
/// Magic, format, kind: authenticated with every chunk.
const AAD_HEADER: usize = 7;
/// The header's random part, the first 16 bytes of every chunk's nonce.
const PREFIX_LEN: usize = 16;
const HEADER_LEN: usize = AAD_HEADER + PREFIX_LEN;
/// Plaintext bytes in every chunk but the last.
pub(crate) const CHUNK: usize = 64 * 1024;
const TAG: usize = 16;

/// What a blob holds; part of what is authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlobKind {
    Outbox = 1,
    Image = 2,
}

/// The key blobs are sealed with.
pub(crate) struct BlobKey {
    cipher: XChaCha20Poly1305,
}

impl BlobKey {
    pub(crate) fn new(state_key: &[u8; 32]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"lazy-blob-key-v1\0");
        hasher.update(state_key);
        let derived = hasher.finalize();
        let key = Key::<XChaCha20Poly1305>::try_from(&derived[..]).expect("a 32-byte key");
        Self {
            cipher: XChaCha20Poly1305::new(&key),
        }
    }

    fn seal(
        &self,
        header: &[u8; HEADER_LEN],
        counter: u64,
        last: bool,
        plaintext: &[u8],
    ) -> io::Result<Vec<u8>> {
        let (nonce, aad) = chunk_params(header, counter, last)?;
        self.cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| io::Error::other("could not encrypt"))
    }

    fn open(
        &self,
        header: &[u8; HEADER_LEN],
        counter: u64,
        last: bool,
        ciphertext: &[u8],
    ) -> io::Result<Vec<u8>> {
        let (nonce, aad) = chunk_params(header, counter, last)?;
        self.cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "the saved copy is damaged"))
    }
}

fn chunk_params(
    header: &[u8; HEADER_LEN],
    counter: u64,
    last: bool,
) -> io::Result<(Nonce<XChaCha20Poly1305>, [u8; AAD_HEADER + 1])> {
    let mut nonce = [0u8; 24];
    nonce[..PREFIX_LEN].copy_from_slice(&header[AAD_HEADER..]);
    nonce[PREFIX_LEN..].copy_from_slice(&counter.to_le_bytes());
    let nonce = Nonce::<XChaCha20Poly1305>::try_from(&nonce[..])
        .map_err(|_| io::Error::other("bad nonce"))?;
    let mut aad = [0u8; AAD_HEADER + 1];
    aad[..AAD_HEADER].copy_from_slice(&header[..AAD_HEADER]);
    aad[AAD_HEADER] = u8::from(last);
    Ok((nonce, aad))
}

/// Writes a blob. Call [`BlobWriter::finish`] to write the last chunk; a blob without it is
/// refused by the reader.
pub(crate) struct BlobWriter<W: Write> {
    out: W,
    key: std::sync::Arc<BlobKey>,
    header: [u8; HEADER_LEN],
    wrote_header: bool,
    buffer: Vec<u8>,
    counter: u64,
}

impl<W: Write> BlobWriter<W> {
    pub(crate) fn new(out: W, key: std::sync::Arc<BlobKey>, kind: BlobKind) -> io::Result<Self> {
        let mut header = [0u8; HEADER_LEN];
        header[..4].copy_from_slice(MAGIC);
        header[4..6].copy_from_slice(&FORMAT.to_le_bytes());
        header[6] = kind as u8;
        getrandom::fill(&mut header[AAD_HEADER..]).map_err(|e| io::Error::other(e.to_string()))?;
        Ok(Self {
            out,
            key,
            header,
            wrote_header: false,
            buffer: Vec::with_capacity(CHUNK + 1),
            counter: 0,
        })
    }

    fn write_header(&mut self) -> io::Result<()> {
        if !self.wrote_header {
            self.out.write_all(&self.header)?;
            self.wrote_header = true;
        }
        Ok(())
    }

    /// Adds plaintext.
    pub(crate) fn push(&mut self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            let room = CHUNK + 1 - self.buffer.len();
            let take = room.min(data.len());
            self.buffer.extend_from_slice(&data[..take]);
            data = &data[take..];
            // A chunk is written only once there is more after it, so the last one is known.
            if self.buffer.len() > CHUNK {
                self.write_header()?;
                let sealed =
                    self.key
                        .seal(&self.header, self.counter, false, &self.buffer[..CHUNK])?;
                self.out.write_all(&sealed)?;
                self.counter += 1;
                self.buffer.drain(..CHUNK);
            }
        }
        Ok(())
    }

    /// Writes the last chunk and returns the output, flushed.
    pub(crate) fn finish(mut self) -> io::Result<W> {
        self.write_header()?;
        let sealed = self
            .key
            .seal(&self.header, self.counter, true, &self.buffer)?;
        self.out.write_all(&sealed)?;
        self.out.flush()?;
        Ok(self.out)
    }
}

/// Reads a blob, one decrypted chunk at a time.
pub(crate) struct BlobReader<R: Read> {
    input: R,
    key: std::sync::Arc<BlobKey>,
    header: [u8; HEADER_LEN],
    /// The chunk after the one being handed out, read ahead to know whether this one is last.
    ahead: Vec<u8>,
    counter: u64,
    done: bool,
}

impl<R: Read> BlobReader<R> {
    pub(crate) fn new(
        mut input: R,
        key: std::sync::Arc<BlobKey>,
        kind: BlobKind,
    ) -> io::Result<Self> {
        let mut header = [0u8; HEADER_LEN];
        input.read_exact(&mut header).map_err(truncated)?;
        if &header[..4] != MAGIC || header[6] != kind as u8 {
            return Err(damaged());
        }
        if u16::from_le_bytes([header[4], header[5]]) != FORMAT {
            return Err(damaged());
        }
        let mut reader = Self {
            input,
            key,
            header,
            ahead: Vec::new(),
            counter: 0,
            done: false,
        };
        reader.ahead = reader.read_sealed()?;
        Ok(reader)
    }

    fn read_sealed(&mut self) -> io::Result<Vec<u8>> {
        let mut sealed = Vec::with_capacity(CHUNK + TAG);
        (&mut self.input)
            .take((CHUNK + TAG) as u64)
            .read_to_end(&mut sealed)?;
        Ok(sealed)
    }

    /// The next piece of plaintext, or `None` at the end.
    pub(crate) fn next_chunk(&mut self) -> io::Result<Option<Vec<u8>>> {
        if self.done {
            return Ok(None);
        }
        let sealed = std::mem::take(&mut self.ahead);
        if sealed.len() < TAG {
            // A blob always has a last chunk, even for an empty file.
            return Err(damaged());
        }
        let next = if sealed.len() == CHUNK + TAG {
            self.read_sealed()?
        } else {
            Vec::new()
        };
        let last = next.is_empty();
        let plain = self.key.open(&self.header, self.counter, last, &sealed)?;
        self.counter += 1;
        self.ahead = next;
        if last {
            self.done = true;
        }
        Ok(Some(plain))
    }
}

fn truncated(_: io::Error) -> io::Error {
    damaged()
}

fn damaged() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "the saved copy is damaged")
}

/// The plaintext size of a blob of `file_len` bytes, if that length can be one.
pub(crate) fn plaintext_len(file_len: u64) -> Option<u64> {
    let body = file_len.checked_sub(HEADER_LEN as u64)?;
    if body < TAG as u64 {
        return None;
    }
    let chunks = body.div_ceil((CHUNK + TAG) as u64);
    body.checked_sub(chunks * TAG as u64)
}

/// Opens the blob file at `path`.
pub(crate) fn open_file(
    path: &std::path::Path,
    key: std::sync::Arc<BlobKey>,
    kind: BlobKind,
) -> io::Result<(BlobReader<io::BufReader<File>>, u64)> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    let size = plaintext_len(len).ok_or_else(damaged)?;
    Ok((BlobReader::new(io::BufReader::new(file), key, kind)?, size))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn key() -> Arc<BlobKey> {
        Arc::new(BlobKey::new(&[9; 32]))
    }

    fn seal(data: &[u8], pieces: usize) -> Vec<u8> {
        let mut writer = BlobWriter::new(Vec::new(), key(), BlobKind::Outbox).unwrap();
        for piece in data.chunks(pieces.max(1)) {
            writer.push(piece).unwrap();
        }
        writer.finish().unwrap()
    }

    fn read_all(bytes: &[u8], kind: BlobKind) -> io::Result<Vec<u8>> {
        let mut reader = BlobReader::new(bytes, key(), kind)?;
        let mut out = Vec::new();
        while let Some(chunk) = reader.next_chunk()? {
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    #[test]
    fn contents_round_trip_at_every_size_that_touches_a_chunk_edge() {
        for len in [
            0,
            1,
            CHUNK - 1,
            CHUNK,
            CHUNK + 1,
            2 * CHUNK,
            2 * CHUNK + 5,
            3 * CHUNK,
        ] {
            let data: Vec<u8> = (0..len).map(|n| (n % 251) as u8).collect();
            // Written in awkward pieces, too.
            for pieces in [1, 777, CHUNK, len.max(1)] {
                let sealed = seal(&data, pieces);
                assert_eq!(
                    read_all(&sealed, BlobKind::Outbox).unwrap(),
                    data,
                    "{len}/{pieces}"
                );
                assert_eq!(
                    plaintext_len(sealed.len() as u64),
                    Some(len as u64),
                    "{len}"
                );
            }
        }
    }

    #[test]
    fn the_file_does_not_contain_its_plaintext() {
        let data = vec![0x41; 1000];
        let sealed = seal(&data, 1000);
        assert!(!sealed.windows(16).any(|w| w == [0x41; 16]));
    }

    #[test]
    fn a_cut_off_blob_is_refused_at_every_length() {
        let data: Vec<u8> = (0..(2 * CHUNK + 10)).map(|n| n as u8).collect();
        let sealed = seal(&data, CHUNK);
        for cut in [
            0,
            5,
            HEADER_LEN - 1,
            HEADER_LEN,
            HEADER_LEN + 3,
            HEADER_LEN + CHUNK + TAG,
            HEADER_LEN + CHUNK + TAG + 1,
            sealed.len() - 1,
        ] {
            assert!(
                read_all(&sealed[..cut], BlobKind::Outbox).is_err(),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn a_dropped_chunk_a_swapped_chunk_and_a_flipped_bit_are_refused() {
        let data: Vec<u8> = (0..(3 * CHUNK)).map(|n| (n % 7) as u8).collect();
        let sealed = seal(&data, CHUNK);
        let stride = CHUNK + TAG;
        // Drop the middle chunk.
        let mut dropped = sealed[..HEADER_LEN + stride].to_vec();
        dropped.extend_from_slice(&sealed[HEADER_LEN + 2 * stride..]);
        assert!(read_all(&dropped, BlobKind::Outbox).is_err());
        // Swap the first two chunks.
        let mut swapped = sealed[..HEADER_LEN].to_vec();
        swapped.extend_from_slice(&sealed[HEADER_LEN + stride..HEADER_LEN + 2 * stride]);
        swapped.extend_from_slice(&sealed[HEADER_LEN..HEADER_LEN + stride]);
        swapped.extend_from_slice(&sealed[HEADER_LEN + 2 * stride..]);
        assert!(read_all(&swapped, BlobKind::Outbox).is_err());
        // Flip a bit anywhere in the body.
        for at in [HEADER_LEN, HEADER_LEN + 100, sealed.len() - 1] {
            let mut flipped = sealed.clone();
            flipped[at] ^= 1;
            assert!(read_all(&flipped, BlobKind::Outbox).is_err(), "bit at {at}");
        }
    }

    #[test]
    fn a_blob_of_another_kind_or_key_is_refused() {
        let sealed = seal(b"hello", 5);
        assert!(read_all(&sealed, BlobKind::Image).is_err());
        let other = Arc::new(BlobKey::new(&[8; 32]));
        let mut reader = BlobReader::new(&sealed[..], other, BlobKind::Outbox).unwrap();
        assert!(reader.next_chunk().is_err());
        // Not a blob at all.
        assert!(
            read_all(
                b"definitely not a blob, but long enough to have a header",
                BlobKind::Outbox
            )
            .is_err()
        );
    }

    #[test]
    fn two_blobs_of_the_same_contents_differ() {
        assert_ne!(seal(b"same", 4), seal(b"same", 4));
    }
}

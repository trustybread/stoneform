//! BLTE: the container every file on the CDN (and in local storage) is
//! wrapped in.
//!
//! ```text
//! "BLTE"  header size (u32 BE; 0: no chunk table)
//! flags (0x0F)  chunk count (u24 BE)
//! per chunk: encoded size (u32 BE), decoded size (u32 BE), MD5 of the encoded chunk
//! chunks: a mode byte, then the data
//! ```
//!
//! Chunks are stored raw (`N`), zlib-compressed (`Z`), as nested BLTE (`F`)
//! or encrypted (`E`, which needs keys this crate doesn't have). A file's
//! encoding key is the MD5 of its header, or of the whole blob when there's
//! no chunk table.

use std::io::{self, Read, Write};

use flate2::{Decompress, FlushDecompress, Status};
use md5::{Digest, Md5};

use crate::bytes::{self, Reader};
use crate::{ContentKey, EncodingKey, Error, Result};

const MAGIC: &[u8; 4] = b"BLTE";

/// Encoded bytes are read this many at a time, so a big chunk never sits in
/// memory whole: WoW's encoding table is a single 110 MB chunk.
const PIECE_SIZE: usize = 1 << 20;

/// Decoded bytes come out of zlib this many at a time.
const INFLATE_BUFFER_SIZE: usize = 256 * 1024;

/// `F` chunks can nest BLTE in BLTE; deeper than this is malformed.
const MAX_NESTING: usize = 4;

/// What a streaming decode produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decoded {
    /// The MD5 of the decoded bytes: the file's content key.
    pub content_key: ContentKey,
    pub size: u64,
}

/// Decodes a small blob (a config, a manifest) in memory.
pub fn decode(blob: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    decode_stream(&mut &blob[..], &mut |piece| {
        out.extend_from_slice(piece);
        Ok(())
    })?;
    Ok(out)
}

/// Decodes `input` into `output` piece by piece, so neither the encoded nor
/// the decoded file is ever held whole. Returns the decoded content's key
/// and size; compare the key with the expected content key.
///
/// Each chunk's MD5 is checked once the chunk has been read, after its bytes
/// went to `output`: on an error, throw away what was written.
pub fn decode_to(mut input: impl Read, mut output: impl Write) -> Result<Decoded> {
    let mut md5 = Md5::new();
    let mut size = 0u64;
    decode_stream(&mut input, &mut |piece| {
        md5.update(piece);
        size += piece.len() as u64;
        output.write_all(piece)?;
        Ok(())
    })?;
    output.flush()?;
    Ok(Decoded { content_key: ContentKey::new(md5.finalize().into()), size })
}

/// Checks a blob exactly as the CDN serves it, without decoding: the header
/// hashes to `key` (the whole blob does, without a chunk table), every
/// chunk's MD5 matches, and the chunks end where the blob does.
pub fn verify(blob: &[u8], key: &EncodingKey) -> Result<()> {
    let mismatch = || Error::ChecksumMismatch(format!("BLTE {key}"));
    let mut r = Reader::new(blob, "BLTE header");
    if &r.array::<4>()? != MAGIC {
        return Err(Error::malformed("BLTE magic"));
    }
    let header_size = r.be(4)? as usize;
    if header_size == 0 {
        return if bytes::md5(blob) == *key.as_bytes() { Ok(()) } else { Err(mismatch()) };
    }
    if header_size > blob.len() || bytes::md5(&blob[..header_size]) != *key.as_bytes() {
        return Err(mismatch());
    }
    let _flags = r.u8()?;
    let chunk_count = r.be(3)? as usize;
    if 12 + 24 * chunk_count > header_size {
        return Err(Error::malformed(format!("BLTE chunk table of {key}")));
    }
    let mut offset = header_size;
    for index in 0..chunk_count {
        let encoded_size = r.be(4)? as usize;
        let _decoded_size = r.be(4)?;
        let expected = r.array::<16>()?;
        let chunk = blob
            .get(offset..offset + encoded_size)
            .ok_or_else(|| Error::malformed(format!("BLTE chunk {index} of {key}")))?;
        if bytes::md5(chunk) != expected {
            return Err(Error::ChecksumMismatch(format!("BLTE {key} chunk {index}")));
        }
        offset += encoded_size;
    }
    if offset != blob.len() {
        return Err(Error::malformed(format!("BLTE length of {key}")));
    }
    Ok(())
}

type Emit<'e> = dyn FnMut(&[u8]) -> Result<()> + 'e;

fn decode_stream(input: &mut dyn Read, emit: &mut Emit<'_>) -> Result<()> {
    let mut buffer = Vec::new();
    decode_nested(input, emit, &mut buffer, 0)
}

fn decode_nested(input: &mut dyn Read, emit: &mut Emit<'_>, buffer: &mut Vec<u8>, depth: usize) -> Result<()> {
    let mut start = [0u8; 8];
    read_exact(input, &mut start)?;
    if &start[..4] != MAGIC {
        return Err(Error::malformed("BLTE magic"));
    }
    let header_size = bytes::be_uint(&start[4..]) as usize;
    if header_size == 0 {
        // No chunk table: the rest is a single chunk, unchecked.
        decode_chunk(input, None, None, emit, buffer, depth)?;
        return Ok(());
    }

    // Read entry by entry rather than `header_size` bytes at once, so a
    // corrupt size can't make this allocate more than the input holds.
    let mut table_start = [0u8; 4];
    read_exact(input, &mut table_start)?;
    let flags = table_start[0];
    if flags != 0x0F {
        return Err(Error::unsupported(format!("BLTE chunk table flags {flags:#04x}")));
    }
    let chunk_count = bytes::be_uint(&table_start[1..]) as usize;
    let extra = header_size.checked_sub(12 + 24 * chunk_count).ok_or_else(|| Error::malformed("BLTE header size"))?;
    let mut chunks = Vec::new();
    for _ in 0..chunk_count {
        let mut entry = [0u8; 24];
        read_exact(input, &mut entry)?;
        let mut md5 = [0u8; 16];
        md5.copy_from_slice(&entry[8..]);
        chunks.push((bytes::be_uint(&entry[..4]), bytes::be_uint(&entry[4..8]), md5));
    }
    // Header bytes past the chunk table (none in practice) are part of the
    // header's hash, not of any chunk.
    io::copy(&mut (&mut *input).take(extra as u64), &mut io::sink())?;
    for (index, (encoded_size, decoded_size, md5)) in chunks.into_iter().enumerate() {
        let produced = decode_chunk(input, Some(encoded_size), Some(md5), emit, buffer, depth)?;
        if produced != decoded_size {
            return Err(Error::malformed(format!("BLTE chunk {index} size")));
        }
    }
    Ok(())
}

/// Decodes one chunk of `size` encoded bytes (`None`: up to the end of the
/// input), reading and emitting it piece by piece. The MD5 is checked at the
/// end. Returns the decoded size.
fn decode_chunk(
    input: &mut dyn Read,
    size: Option<u64>,
    expected: Option<[u8; 16]>,
    emit: &mut Emit<'_>,
    buffer: &mut Vec<u8>,
    depth: usize,
) -> Result<u64> {
    let mut md5 = Md5::new();
    let mut remaining = size.unwrap_or(u64::MAX);
    let mut mode = None;
    let mut inflater = None;
    let mut nested = Vec::new(); // `F` chunks: nested BLTE, rare and small
    let mut produced = 0u64;
    while remaining > 0 {
        let want = remaining.min(PIECE_SIZE as u64) as usize;
        if buffer.len() < want {
            buffer.resize(want, 0);
        }
        let read = read_up_to(input, &mut buffer[..want])?;
        if read == 0 {
            if size.is_some() {
                return Err(Error::malformed("BLTE (unexpected end)"));
            }
            break;
        }
        remaining -= read as u64;
        md5.update(&buffer[..read]);
        let mut body = &buffer[..read];
        let chunk_mode = match mode {
            Some(mode) => mode,
            None => {
                let first = body[0];
                body = &body[1..];
                match first {
                    b'N' | b'F' => {}
                    b'Z' => inflater = Some(Inflater::new()),
                    b'E' => return Err(Error::unsupported("BLTE encrypted content")),
                    other => return Err(Error::unsupported(format!("BLTE chunk mode {other:#04x}"))),
                }
                mode = Some(first);
                first
            }
        };
        match (chunk_mode, inflater.as_mut()) {
            (b'Z', Some(inflater)) => produced += inflater.feed(body, emit)?,
            (b'N', _) if !body.is_empty() => {
                emit(body)?;
                produced += body.len() as u64;
            }
            (b'F', _) => nested.extend_from_slice(body),
            _ => {}
        }
    }
    if let Some(expected) = expected {
        if <[u8; 16]>::from(md5.finalize()) != expected {
            return Err(Error::ChecksumMismatch("BLTE chunk".into()));
        }
    }
    if let Some(inflater) = inflater {
        inflater.finish()?;
    }
    if mode == Some(b'F') {
        if depth >= MAX_NESTING {
            return Err(Error::malformed("BLTE (nested too deeply)"));
        }
        let mut nested_buffer = Vec::new();
        decode_nested(
            &mut nested.as_slice(),
            &mut |piece| {
                produced += piece.len() as u64;
                emit(piece)
            },
            &mut nested_buffer,
            depth + 1,
        )?;
    }
    Ok(produced)
}

/// Fills `buf` unless the input ends first; returns how much it read.
fn read_up_to(input: &mut dyn Read, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match input.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(filled)
}

fn read_exact(input: &mut dyn Read, buf: &mut [u8]) -> Result<()> {
    if read_up_to(input, buf)? == buf.len() { Ok(()) } else { Err(Error::malformed("BLTE (unexpected end)")) }
}

/// A zlib stream fed piece by piece.
struct Inflater {
    stream: Decompress,
    out: Vec<u8>,
    ended: bool,
}

impl Inflater {
    fn new() -> Self {
        Self { stream: Decompress::new(true), out: vec![0; INFLATE_BUFFER_SIZE], ended: false }
    }

    /// Inflates `input`, handing the output to `emit`. Returns its size.
    /// Bytes after the end of the zlib stream are ignored.
    fn feed(&mut self, mut input: &[u8], emit: &mut Emit<'_>) -> Result<u64> {
        let mut total = 0u64;
        while !self.ended {
            let (in_before, out_before) = (self.stream.total_in(), self.stream.total_out());
            let status = self
                .stream
                .decompress(input, &mut self.out, FlushDecompress::None)
                .map_err(|error| Error::malformed(format!("zlib data ({error})")))?;
            let consumed = (self.stream.total_in() - in_before) as usize;
            let produced = (self.stream.total_out() - out_before) as usize;
            input = &input[consumed..];
            if produced > 0 {
                emit(&self.out[..produced])?;
                total += produced as u64;
            }
            match status {
                Status::StreamEnd => self.ended = true,
                // Out of input with room left in the output: wait for the next piece.
                Status::BufError => break,
                Status::Ok if input.is_empty() && produced < self.out.len() => break,
                Status::Ok => {}
            }
        }
        Ok(total)
    }

    fn finish(&self) -> Result<()> {
        if self.ended { Ok(()) } else { Err(Error::malformed("zlib data (truncated)")) }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use flate2::Compression;
    use flate2::write::ZlibEncoder;

    use super::*;

    pub(crate) fn zlib(data: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    pub(crate) enum Chunk<'a> {
        Raw(&'a [u8]),
        Zlib(&'a [u8]),
    }

    impl Chunk<'_> {
        fn plain(&self) -> &[u8] {
            match self {
                Chunk::Raw(data) | Chunk::Zlib(data) => data,
            }
        }

        fn encoded(&self) -> Vec<u8> {
            match self {
                Chunk::Raw(data) => [b"N", *data].concat(),
                Chunk::Zlib(data) => [b"Z".to_vec(), zlib(data)].concat(),
            }
        }
    }

    /// A BLTE blob with a chunk table, as the CDN serves them.
    pub(crate) fn blte(chunks: &[Chunk<'_>]) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        bytes::push_be(&mut out, 12 + 24 * chunks.len() as u64, 4);
        out.push(0x0F);
        bytes::push_be(&mut out, chunks.len() as u64, 3);
        for chunk in chunks {
            let encoded = chunk.encoded();
            bytes::push_be(&mut out, encoded.len() as u64, 4);
            bytes::push_be(&mut out, chunk.plain().len() as u64, 4);
            out.extend_from_slice(&bytes::md5(&encoded));
        }
        for chunk in chunks {
            out.extend(chunk.encoded());
        }
        out
    }

    /// Port of Waypoint's `blteDecodesRawAndZlibChunks`, streaming path included.
    #[test]
    fn decodes_raw_and_zlib_chunks() {
        let a = b"hello ".to_vec();
        let b = "world ".repeat(5000).into_bytes();
        let encoded = blte(&[Chunk::Raw(&a), Chunk::Zlib(&b)]);
        let plain = [a, b].concat();
        assert_eq!(decode(&encoded).unwrap(), plain);

        let mut out = Vec::new();
        let decoded = decode_to(encoded.as_slice(), &mut out).unwrap();
        assert_eq!(out, plain);
        assert_eq!(decoded, Decoded { content_key: ContentKey::of(&plain), size: plain.len() as u64 });
    }

    /// A zlib chunk bigger than a read piece and an inflate buffer decodes
    /// the same through a reader that hands out a few bytes at a time.
    #[test]
    fn streams_large_chunks_through_small_reads() {
        struct Trickle<'a>(&'a [u8]);
        impl Read for Trickle<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let n = buf.len().min(self.0.len()).min(777);
                buf[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                Ok(n)
            }
        }
        let plain: Vec<u8> = (0..3 * PIECE_SIZE).map(|i| (i % 251) as u8 ^ (i >> 12) as u8).collect();
        let encoded = blte(&[Chunk::Zlib(&plain), Chunk::Raw(b"tail")]);
        let mut out = Vec::new();
        let decoded = decode_to(Trickle(&encoded), &mut out).unwrap();
        assert_eq!(decoded.size, plain.len() as u64 + 4);
        assert_eq!(&out[..plain.len()], plain.as_slice());
    }

    /// Port of Waypoint's `blteWithoutChunkTable`.
    #[test]
    fn decodes_without_chunk_table() {
        let encoded = [b"BLTE\0\0\0\0Z".to_vec(), zlib(b"single")].concat();
        assert_eq!(decode(&encoded).unwrap(), b"single");
    }

    #[test]
    fn decodes_nested_blte() {
        let inner = blte(&[Chunk::Raw(b"inner "), Chunk::Zlib(b"content")]);
        let mut encoded = b"BLTE\0\0\0\0F".to_vec();
        encoded.extend(inner);
        assert_eq!(decode(&encoded).unwrap(), b"inner content");
    }

    /// Port of Waypoint's `blteRejectsCorruptChunks`.
    #[test]
    fn rejects_corrupt_chunks() {
        let mut encoded = blte(&[Chunk::Raw(b"payload")]);
        *encoded.last_mut().unwrap() ^= 0xFF;
        assert!(matches!(decode(&encoded), Err(Error::ChecksumMismatch(_))));
        let truncated = blte(&[Chunk::Raw(b"payload")]);
        assert!(matches!(decode(&truncated[..truncated.len() - 1]), Err(Error::Malformed(_))));
        assert!(matches!(decode(b"BLTX\0\0\0\0N"), Err(Error::Malformed(_))));
    }

    /// Port of Waypoint's `blteRefusesEncryptedChunks`.
    #[test]
    fn refuses_encrypted_chunks() {
        let encoded = [b"BLTE\0\0\0\0E".as_slice(), &[0; 8]].concat();
        assert!(matches!(decode(&encoded), Err(Error::Unsupported(_))));
    }

    #[test]
    fn refuses_other_chunk_table_flags_and_wrong_sizes() {
        let mut encoded = blte(&[Chunk::Raw(b"payload")]);
        encoded[8] = 0x10;
        assert!(matches!(decode(&encoded), Err(Error::Unsupported(_))));
        let mut encoded = blte(&[Chunk::Raw(b"payload")]);
        encoded[19] += 1; // the decoded size the table claims
        assert!(matches!(decode(&encoded), Err(Error::Malformed(_))));
        // A header claiming 4 GB fails on the data, without allocating it.
        let mut hostile = blte(&[Chunk::Raw(b"payload")]);
        hostile[4..8].copy_from_slice(&[0xFF; 4]);
        assert!(matches!(decode(&hostile), Err(Error::Malformed(_))));
        let mut short = blte(&[Chunk::Raw(b"payload")]);
        short[7] = 20; // smaller than its chunk table
        assert!(matches!(decode(&short), Err(Error::Malformed(_))));
    }

    /// Port of Waypoint's `verifiesBlobsWithoutDecodingThem`.
    #[test]
    fn verifies_blobs_without_decoding_them() {
        let blob = blte(&[Chunk::Raw(b"hello")]);
        let key = EncodingKey::new(bytes::md5(&blob[..36]));
        verify(&blob, &key).unwrap();

        let mut bad_chunk = blob.clone();
        *bad_chunk.last_mut().unwrap() ^= 1;
        assert!(matches!(verify(&bad_chunk, &key), Err(Error::ChecksumMismatch(_))));
        assert!(matches!(verify(&blob, &EncodingKey::new([0; 16])), Err(Error::ChecksumMismatch(_))));
        assert!(matches!(verify(&[blob.as_slice(), &[0]].concat(), &key), Err(Error::Malformed(_))));

        // Without a chunk table the whole blob is hashed.
        let single = b"BLTE\0\0\0\0Nraw".to_vec();
        verify(&single, &EncodingKey::new(bytes::md5(&single))).unwrap();
        assert!(verify(&single, &key).is_err());
    }
}

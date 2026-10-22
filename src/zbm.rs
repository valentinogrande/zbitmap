//! LZBITMAP decompressor.
//!
//! A Rust port of `zbm_decompress` from [libzbitmap] (MIT, Corellium LLC,
//! Ernesto A. Fernández). It follows the same algorithm and the same bounds
//! checks as the original, with pointers replaced by indices into the chunk.
//!
//! An LZBITMAP buffer is laid out as:
//!
//! - the magic `ZBM\x09`;
//! - a sequence of chunks, each starting with `len: u24` (total chunk length)
//!   and `decmp_len: u24` (decompressed length, at most `0x8000`). The last
//!   chunk has `decmp_len == 0`;
//! - an uncompressed chunk has `len == decmp_len + 6` and its payload follows
//!   the header;
//! - a compressed chunk has a 15-byte header holding three `u24` offsets to
//!   metadata areas, and ends with 17 bytes encoding twelve 10-bit bitmaps.
//!
//! [libzbitmap]: https://github.com/eafer/libzbitmap

use std::fmt;

const MAGIC: &[u8; 4] = b"ZBM\x09";
const MAX_DECMP_CHUNK_SIZE: usize = 0x8000;
const CHUNK_HDR: usize = 6;
const CMP_CHUNK_HDR: usize = 15;

/// Indices 0-2 and 0xf are not real bitmap indices.
const BITMAP_COUNT: usize = 16 - 1 - 3;
const BITMAP_BASE: u8 = 3;
const BITMAP_BYTECNT: usize = 17;
const MAX_PERIOD_BYTECNT: u8 = 2;

/// Errors returned by [`decompress`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The compressed input is malformed (libzbitmap's `ZBM_INVAL`).
    Invalid(&'static str),
    /// The output would exceed the requested limit (libzbitmap's `ZBM_RANGE`).
    Range,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Invalid(why) => write!(f, "invalid LZBITMAP data: {why}"),
            Error::Range => write!(f, "LZBITMAP output exceeds the expected size"),
        }
    }
}

impl std::error::Error for Error {}

fn u24(b: &[u8]) -> usize {
    b[0] as usize | (b[1] as usize) << 8 | (b[2] as usize) << 16
}

/// Decompresses an LZBITMAP buffer, appending the result to `out`.
///
/// Fails with [`Error::Range`] if more than `limit` bytes would be produced.
/// Back-references never reach into data that was already in `out` before the
/// call. Returns the number of bytes appended.
///
/// ```
/// // "hello" stored as one uncompressed chunk, followed by the end chunk.
/// let src = b"ZBM\x09\x0b\x00\x00\x05\x00\x00hello\x06\x00\x00\x00\x00\x00";
/// let mut out = Vec::new();
/// assert_eq!(zbitmap::decompress(src, &mut out, 5), Ok(5));
/// assert_eq!(out, b"hello");
/// ```
pub fn decompress(src: &[u8], out: &mut Vec<u8>, limit: usize) -> Result<usize, Error> {
    if src.len() < MAGIC.len() || &src[..MAGIC.len()] != MAGIC {
        return Err(Error::Invalid("bad magic"));
    }
    let base = out.len();
    let mut pos = MAGIC.len();

    loop {
        let rest = &src[pos..];
        if rest.len() < CHUNK_HDR {
            return Err(Error::Invalid("truncated chunk header"));
        }
        let len = u24(&rest[0..3]);
        if len > rest.len() {
            return Err(Error::Invalid("chunk longer than input"));
        }
        let decmp_len = u24(&rest[3..6]);
        if decmp_len > MAX_DECMP_CHUNK_SIZE {
            return Err(Error::Invalid("decompressed chunk larger than 0x8000"));
        }
        if out.len() - base + decmp_len > limit {
            return Err(Error::Range);
        }

        let chunk = &rest[..len];
        if len == decmp_len + CHUNK_HDR {
            out.extend_from_slice(&chunk[CHUNK_HDR..]);
        } else {
            ChunkDecoder::new(chunk, decmp_len, out, base)?.run()?;
        }

        pos += len;
        if decmp_len == 0 {
            break;
        }
    }
    Ok(out.len() - base)
}

#[derive(Clone, Copy, Default)]
struct Bitmap {
    bitmap: u8,
    period_bytecnt: u8,
}

struct ChunkDecoder<'a, 'o> {
    c: &'a [u8],
    out: &'o mut Vec<u8>,
    base: usize,
    decmp_len: usize,
    written: usize,
    period: usize,
    data: usize,
    meta_1: usize,
    meta_2: usize,
    /// Nibble pointer into metadata area 3: byte index and whether the high
    /// nibble is next.
    meta_3: usize,
    meta_3_high: bool,
    bitmaps: [Bitmap; BITMAP_COUNT],
}

impl<'a, 'o> ChunkDecoder<'a, 'o> {
    fn new(
        c: &'a [u8],
        decmp_len: usize,
        out: &'o mut Vec<u8>,
        base: usize,
    ) -> Result<Self, Error> {
        let len = c.len();
        if len < CMP_CHUNK_HDR {
            return Err(Error::Invalid("truncated compressed chunk"));
        }
        let off1 = u24(&c[6..9]);
        let off2 = u24(&c[9..12]);
        let off3 = u24(&c[12..15]);
        if off1 >= len || off2 >= len || off3 >= len {
            return Err(Error::Invalid("metadata offset outside chunk"));
        }
        let mut d = Self {
            c,
            out,
            base,
            decmp_len,
            written: 0,
            period: 8,
            data: CMP_CHUNK_HDR,
            meta_1: off1,
            meta_2: off2,
            meta_3: off3,
            meta_3_high: false,
            bitmaps: [Bitmap::default(); BITMAP_COUNT],
        };
        d.read_bitmaps()?;
        Ok(d)
    }

    fn run(mut self) -> Result<(), Error> {
        while self.written < self.decmp_len {
            self.decompress_single_bitmap()?;
        }
        Ok(())
    }

    /// The twelve 8-bit bitmaps, each followed by 2 bits of `period_bytecnt`,
    /// live in the last 17 bytes of the chunk.
    fn read_bitmaps(&mut self) -> Result<(), Error> {
        let len = self.c.len();
        if len < BITMAP_BYTECNT {
            return Err(Error::Invalid("chunk too short for bitmaps"));
        }
        let mut addr = len - BITMAP_BYTECNT;
        let mut bit = 0u32;
        let mut read_bit = |c: &[u8]| -> Result<u8, Error> {
            if addr >= len {
                return Err(Error::Invalid("truncated bitmaps"));
            }
            let r = c[addr] >> bit & 1;
            bit += 1;
            if bit == 8 {
                bit = 0;
                addr += 1;
            }
            Ok(r)
        };
        for slot in self.bitmaps.iter_mut() {
            let mut bm = Bitmap::default();
            for j in 0..8 {
                bm.bitmap |= read_bit(self.c)? << j;
            }
            for j in 0..2 {
                bm.period_bytecnt |= read_bit(self.c)? << j;
            }
            if bm.period_bytecnt > MAX_PERIOD_BYTECNT {
                return Err(Error::Invalid("period byte count larger than 2"));
            }
            *slot = bm;
        }
        Ok(())
    }

    fn read_nibble(&mut self) -> Result<u8, Error> {
        if self.meta_3 >= self.c.len() {
            return Err(Error::Invalid("ran out of nibbles"));
        }
        let b = self.c[self.meta_3];
        if !self.meta_3_high {
            self.meta_3_high = true;
            Ok(b & 0xf)
        } else {
            self.meta_3_high = false;
            self.meta_3 += 1;
            Ok(b >> 4)
        }
    }

    fn rewind_nibble(&mut self) {
        if !self.meta_3_high {
            self.meta_3_high = true;
            self.meta_3 -= 1;
        } else {
            self.meta_3_high = false;
        }
    }

    fn apply_bitmap(&mut self, bm: Bitmap) -> Result<(), Error> {
        let len = self.c.len();
        // New repetition periods come from metadata area 1.
        if bm.period_bytecnt != 0 {
            self.period = 0;
            for i in 0..bm.period_bytecnt as usize {
                if self.meta_1 >= len {
                    return Err(Error::Invalid("ran out of periods"));
                }
                self.period |= (self.c[self.meta_1] as usize) << (i * 8);
                self.meta_1 += 1;
            }
        }
        if self.period == 0 {
            return Err(Error::Invalid("zero period"));
        }

        for i in 0..8 {
            if self.written == self.decmp_len {
                break;
            }
            if bm.bitmap & (1 << i) != 0 {
                if self.data >= len {
                    return Err(Error::Invalid("ran out of literals"));
                }
                self.out.push(self.c[self.data]);
                self.data += 1;
            } else {
                if self.out.len() - self.base < self.period {
                    return Err(Error::Invalid("back-reference before start of output"));
                }
                let b = self.out[self.out.len() - self.period];
                self.out.push(b);
            }
            self.written += 1;
        }
        Ok(())
    }

    fn apply_bitmap_number(&mut self, n: u8) -> Result<(), Error> {
        if n == 0xf {
            return Err(Error::Invalid("bitmap number 0xf"));
        }
        if n > MAX_PERIOD_BYTECNT {
            let bm = self.bitmaps[(n - BITMAP_BASE) as usize];
            return self.apply_bitmap(bm);
        }
        // 0..=2: the bitmap is stored explicitly in area 2 and `n` is its
        // period byte count.
        if self.meta_2 >= self.c.len() {
            return Err(Error::Invalid("ran out of explicit bitmaps"));
        }
        let bm = Bitmap {
            bitmap: self.c[self.meta_2],
            period_bytecnt: n,
        };
        self.meta_2 += 1;
        self.apply_bitmap(bm)
    }

    fn read_repetition_count(&mut self) -> Result<u16, Error> {
        // Don't mistake the trailing bitmaps for a repetition count.
        if self.decmp_len - self.written <= 8 {
            return Ok(1);
        }
        let mut nibble = self.read_nibble()?;
        if nibble != 0xf {
            self.rewind_nibble();
            return Ok(1);
        }
        // Repeating 3 times would save no space, so counts start at 4.
        let mut total: u16 = 4;
        while nibble == 0xf {
            nibble = self.read_nibble()?;
            total = total
                .checked_add(nibble as u16)
                .ok_or(Error::Invalid("repetition count overflow"))?;
        }
        Ok(total)
    }

    fn decompress_single_bitmap(&mut self) -> Result<(), Error> {
        let n = self.read_nibble()?;
        let repeat = self.read_repetition_count()?;
        for _ in 0..repeat {
            self.apply_bitmap_number(n)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(len: usize, decmp: usize) -> Vec<u8> {
        let mut v = Vec::new();
        for x in [len, decmp] {
            v.extend_from_slice(&[(x & 0xff) as u8, (x >> 8 & 0xff) as u8, (x >> 16) as u8]);
        }
        v
    }

    #[test]
    fn rejects_bad_magic() {
        let mut out = Vec::new();
        assert_eq!(
            decompress(b"ZBM\x08", &mut out, 100),
            Err(Error::Invalid("bad magic"))
        );
    }

    #[test]
    fn uncompressed_chunk_then_terminator() {
        let mut src = MAGIC.to_vec();
        src.extend(hdr(6 + 5, 5));
        src.extend_from_slice(b"hello");
        src.extend(hdr(6, 0));
        let mut out = b"prefix".to_vec();
        assert_eq!(decompress(&src, &mut out, 5), Ok(5));
        assert_eq!(out, b"prefixhello");
    }

    #[test]
    fn enforces_limit() {
        let mut src = MAGIC.to_vec();
        src.extend(hdr(6 + 5, 5));
        src.extend_from_slice(b"hello");
        src.extend(hdr(6, 0));
        assert_eq!(decompress(&src, &mut Vec::new(), 4), Err(Error::Range));
    }

    #[test]
    fn truncated_input_is_error_not_panic() {
        let mut src = MAGIC.to_vec();
        src.extend(hdr(6 + 5, 5));
        src.extend_from_slice(b"hel");
        assert!(decompress(&src, &mut Vec::new(), 100).is_err());
        for cut in 0..src.len() {
            let _ = decompress(&src[..cut], &mut Vec::new(), 100);
        }
    }

    /// Hand-built compressed chunk: 16 bytes = "ab" followed by 14 copies with
    /// period 2.
    /// - explicit bitmap (number 1 = one period byte) 0b0000_0011: 2 literals
    ///   and 6 copies, period read from meta_1 = 2;
    /// - number 3 (bitmaps[0] = 0x00, byte count 0): 8 more copies.
    fn hand_built() -> Vec<u8> {
        let mut body = Vec::new();
        let data_off = CMP_CHUNK_HDR;
        body.extend_from_slice(b"ab");
        let m1 = data_off + body.len();
        body.push(2);
        let m2 = data_off + body.len();
        body.push(0b0000_0011);
        let m3 = data_off + body.len();
        body.push(0x31); // low nibble 1, high nibble 3
        body.extend_from_slice(&[0u8; BITMAP_BYTECNT]);
        let len = CMP_CHUNK_HDR + body.len();

        let mut src = MAGIC.to_vec();
        src.extend(hdr(len, 16));
        for off in [m1, m2, m3] {
            src.extend_from_slice(&[off as u8, 0, 0]);
        }
        src.extend(body);
        src.extend(hdr(6, 0));
        src
    }

    #[test]
    fn hand_built_compressed_chunk() {
        let mut out = Vec::new();
        assert_eq!(decompress(&hand_built(), &mut out, 16), Ok(16));
        assert_eq!(out, b"abababababababab");
    }

    #[test]
    fn corrupted_compressed_chunk_never_panics() {
        let src = hand_built();
        for i in 0..src.len() {
            for flip in [0x01u8, 0x10, 0x80, 0xff] {
                let mut s = src.clone();
                s[i] ^= flip;
                let _ = decompress(&s, &mut Vec::new(), 1 << 16);
            }
        }
    }
}

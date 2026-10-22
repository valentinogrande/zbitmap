//! APFS/HFS+ transparent compression (decmpfs), LZBITMAP types only.
//!
//! Layout as implemented by `compress.c` in [linux-apfs-rw]:
//!
//! - the `com.apple.decmpfs` xattr holds the magic `fpmc`, `algo: u32 LE` and
//!   `size: u64 LE` (uncompressed size). For `*_ATTR` types the compressed
//!   data follows inline.
//! - type 14 (`LZBITMAP_RSRC`): the resource fork starts with a table of
//!   `n + 1` little-endian `u32` offsets; block `i` spans
//!   `[table[i], table[i + 1])` and decompresses to 64 KiB (the last one to the
//!   remainder).
//! - each block starting with `0x5a` (`'Z'`) is LZBITMAP; if
//!   `(b0 & 0x0f) == 0x0f`, the rest of the block is stored uncompressed.
//!
//! [linux-apfs-rw]: https://github.com/linux-apfs/linux-apfs-rw

use std::fmt;
use std::io::{self, Read, Seek, SeekFrom, Write};

use crate::zbm;

/// Size of the decmpfs header at the start of the `com.apple.decmpfs` xattr.
pub const HEADER_LEN: usize = 16;
/// Uncompressed size of each resource-fork block.
pub const BLOCK_SIZE: u64 = 0x10000;
/// LZBITMAP data stored inline in the xattr.
pub const LZBITMAP_ATTR: u32 = 13;
/// LZBITMAP data stored in the resource fork.
pub const LZBITMAP_RSRC: u32 = 14;

/// Errors returned while decoding decmpfs data.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    BadHeader(&'static str),
    Unsupported(u32),
    BadTable(String),
    Block { index: u64, reason: String },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::BadHeader(why) => write!(f, "invalid decmpfs header: {why}"),
            Error::Unsupported(t) => write!(f, "unsupported decmpfs type {t} (only 13 and 14)"),
            Error::BadTable(why) => write!(f, "invalid block table: {why}"),
            Error::Block { index, reason } => write!(f, "block {index}: {reason}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

/// Parsed decmpfs header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Compression type.
    pub algo: u32,
    /// Uncompressed file size.
    pub size: u64,
}

/// Parses the 16-byte header at the start of a `com.apple.decmpfs` xattr.
pub fn parse_header(b: &[u8]) -> Result<Header, Error> {
    if b.len() < HEADER_LEN {
        return Err(Error::BadHeader("shorter than 16 bytes"));
    }
    if &b[0..4] != b"fpmc" {
        return Err(Error::BadHeader("magic is not fpmc"));
    }
    Ok(Header {
        algo: u32::from_le_bytes(b[4..8].try_into().unwrap()),
        size: u64::from_le_bytes(b[8..16].try_into().unwrap()),
    })
}

/// Decodes one block and checks that it produces exactly `expected` bytes.
fn decode_block(index: u64, cdata: &[u8], expected: usize, out: &mut Vec<u8>) -> Result<(), Error> {
    let err = |reason: String| Error::Block { index, reason };
    out.clear();
    match cdata.first() {
        None => return Err(err("empty block".into())),
        Some(0x5a) => {
            zbm::decompress(cdata, out, expected).map_err(|e| err(e.to_string()))?;
        }
        Some(b) if b & 0x0f == 0x0f => out.extend_from_slice(&cdata[1..]),
        Some(b) => return Err(err(format!("unknown block marker {b:#04x}"))),
    }
    if out.len() != expected {
        return Err(err(format!(
            "produced {} bytes, expected {expected}",
            out.len()
        )));
    }
    Ok(())
}

/// Decodes a type 13 file, whose data is inline in the xattr (single block).
pub fn decode_attr<W: Write>(xattr: &[u8], mut w: W) -> Result<u64, Error> {
    let hdr = parse_header(xattr)?;
    if hdr.algo != LZBITMAP_ATTR {
        return Err(Error::Unsupported(hdr.algo));
    }
    if hdr.size > BLOCK_SIZE {
        return Err(Error::BadHeader("type 13 larger than one block"));
    }
    let mut out = Vec::new();
    decode_block(0, &xattr[HEADER_LEN..], hdr.size as usize, &mut out)?;
    w.write_all(&out)?;
    Ok(hdr.size)
}

/// Decodes a type 14 file from its resource fork, block by block.
///
/// Memory use is bounded by one block, so multi-gigabyte files are fine.
/// Returns the number of bytes written, which always equals `hdr.size`.
pub fn decode_rsrc<R: Read + Seek, W: Write>(
    hdr: Header,
    mut rsrc: R,
    mut w: W,
) -> Result<u64, Error> {
    if hdr.algo != LZBITMAP_RSRC {
        return Err(Error::Unsupported(hdr.algo));
    }
    let rsrc_len = rsrc.seek(SeekFrom::End(0))?;
    let n = hdr.size.div_ceil(BLOCK_SIZE);
    let table_len = (n + 1) * 4;
    if table_len > rsrc_len {
        return Err(Error::BadTable(format!(
            "{n} blocks do not fit in {rsrc_len} bytes"
        )));
    }

    rsrc.seek(SeekFrom::Start(0))?;
    let mut raw = vec![0u8; table_len as usize];
    rsrc.read_exact(&mut raw)?;
    let table: Vec<u64> = raw
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()) as u64)
        .collect();
    if table[0] != table_len {
        return Err(Error::BadTable(format!(
            "first offset {} != table size {table_len}",
            table[0]
        )));
    }
    if table.windows(2).any(|p| p[1] < p[0]) || table[n as usize] > rsrc_len {
        return Err(Error::BadTable(
            "offsets not monotonic or past end of fork".into(),
        ));
    }

    let mut cdata = Vec::new();
    let mut out = Vec::with_capacity(BLOCK_SIZE as usize);
    let mut total = 0u64;
    for i in 0..n {
        let (start, end) = (table[i as usize], table[i as usize + 1]);
        cdata.resize((end - start) as usize, 0);
        rsrc.seek(SeekFrom::Start(start))?;
        rsrc.read_exact(&mut cdata)?;
        let expected = (hdr.size - i * BLOCK_SIZE).min(BLOCK_SIZE) as usize;
        decode_block(i, &cdata, expected, &mut out)?;
        w.write_all(&out)?;
        total += out.len() as u64;
    }
    w.flush()?;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn header(algo: u32, size: u64) -> Vec<u8> {
        let mut h = b"fpmc".to_vec();
        h.extend_from_slice(&algo.to_le_bytes());
        h.extend_from_slice(&size.to_le_bytes());
        h
    }

    #[test]
    fn parses_header() {
        let h = parse_header(&header(14, 123)).unwrap();
        assert_eq!(
            h,
            Header {
                algo: 14,
                size: 123
            }
        );
        assert!(parse_header(b"xpmc000000000000").is_err());
    }

    /// Two uncompressed blocks (marker 0xff): 64 KiB + 10 bytes.
    #[test]
    fn rsrc_with_raw_blocks() {
        let size = BLOCK_SIZE + 10;
        let b0: Vec<u8> = (0..BLOCK_SIZE).map(|i| i as u8).collect();
        let b1 = vec![7u8; 10];
        let table_len = 3 * 4u32;
        let o1 = table_len + 1 + b0.len() as u32;
        let o2 = o1 + 1 + b1.len() as u32;
        let mut fork = Vec::new();
        for o in [table_len, o1, o2] {
            fork.extend_from_slice(&o.to_le_bytes());
        }
        fork.push(0xff);
        fork.extend(&b0);
        fork.push(0xff);
        fork.extend(&b1);

        let mut out = Vec::new();
        let n = decode_rsrc(Header { algo: 14, size }, Cursor::new(fork), &mut out).unwrap();
        assert_eq!(n, size);
        assert_eq!(&out[..BLOCK_SIZE as usize], &b0[..]);
        assert_eq!(&out[BLOCK_SIZE as usize..], &b1[..]);
    }

    #[test]
    fn attr_inline_raw() {
        let mut x = header(13, 3);
        x.extend_from_slice(&[0xff, b'a', b'b', b'c']);
        let mut out = Vec::new();
        assert_eq!(decode_attr(&x, &mut out).unwrap(), 3);
        assert_eq!(out, b"abc");
    }

    #[test]
    fn rejects_wrong_block_size() {
        let mut fork = [8u32.to_le_bytes(), 12u32.to_le_bytes()].concat();
        fork.extend_from_slice(&[0xff, 1, 2, 3]);
        let r = decode_rsrc(Header { algo: 14, size: 5 }, Cursor::new(fork), Vec::new());
        assert!(matches!(r, Err(Error::Block { index: 0, .. })));
    }

    #[test]
    fn rejects_other_types() {
        let r = decode_rsrc(Header { algo: 8, size: 1 }, Cursor::new(vec![]), Vec::new());
        assert!(matches!(r, Err(Error::Unsupported(8))));
    }
}

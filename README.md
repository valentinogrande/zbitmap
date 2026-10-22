# zbitmap

[![crates.io](https://img.shields.io/crates/v/zbitmap.svg)](https://crates.io/crates/zbitmap)
[![docs.rs](https://docs.rs/zbitmap/badge.svg)](https://docs.rs/zbitmap)

Pure-Rust **LZBITMAP** decompressor, plus a decoder for APFS transparent compression
(**decmpfs** types 13 and 14). No dependencies, no `unsafe`.

LZBITMAP is the format Apple has used for APFS transparent compression since macOS 12, notably
for the system `dyld_shared_cache`. Existing Rust APFS/decmpfs crates handle zlib, LZVN and LZFSE
but not LZBITMAP; this crate fills that gap.

## Library

```rust
// Raw LZBITMAP buffer (starts with the magic "ZBM\x09").
let mut out = Vec::new();
zbitmap::decompress(&compressed, &mut out, expected_len)?;
```

```rust
use std::fs::File;
use zbitmap::decmpfs;

// A whole file stored as decmpfs type 14 (data in the resource fork).
let hdr = decmpfs::parse_header(&decmpfs_xattr)?;
let written = decmpfs::decode_rsrc(hdr, File::open("file.rsrc")?, File::create("file.out")?)?;
assert_eq!(written, hdr.size);
```

`decode_rsrc` streams one 64 KiB block at a time, so multi-gigabyte files use constant memory.
Every block is checked against its expected size; a mismatch is an error, never silently
truncated output. Malformed input returns an error and never panics.

## CLI

`decmpfs-decode` rebuilds files from the side files that extractors such as 7-Zip produce for
compressed APFS entries (an empty `X`, plus `X:com.apple.decmpfs` and `X:com.apple.ResourceFork`):

```sh
cargo install zbitmap
decmpfs-decode path/to/X X.decoded
```

## Validation

- Unit tests with hand-built LZBITMAP chunks, raw blocks, truncated and bit-flipped input.
- Decoded the complete macOS 27.0.1 arm64e `dyld_shared_cache` (82 files, 6.6 GB): every block
  matched its declared size, and the result opens cleanly in
  [`ipsw dyld info`](https://github.com/blacktop/ipsw) (4088 images, 79 subcaches).
  No Apple data is included in this repository.

## Limitations

- Decompression only; there is no compressor.
- decmpfs support covers LZBITMAP types 13 and 14 only. For zlib, LZVN and LZFSE use other crates.

## Credits and license

The decompressor is a port of [libzbitmap](https://github.com/eafer/libzbitmap) by Ernesto A.
Fernández for Corellium LLC (MIT). The decmpfs resource-fork layout follows
[linux-apfs-rw](https://github.com/linux-apfs/linux-apfs-rw).

MIT. See [LICENSE](LICENSE).

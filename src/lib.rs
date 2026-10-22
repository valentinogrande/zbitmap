//! Pure-Rust LZBITMAP decompression, with no dependencies and no `unsafe`.
//!
//! LZBITMAP is the compression format Apple uses for APFS transparent
//! compression (decmpfs types 13 and 14) since macOS 12, including the
//! system's `dyld_shared_cache`.
//!
//! - [`decompress`]: decodes a raw LZBITMAP buffer (magic `ZBM\x09`).
//! - [`decmpfs`]: decodes whole files from their `com.apple.decmpfs` xattr and
//!   resource fork.
//!
//! The decompressor is a port of [libzbitmap] by Corellium (MIT).
//!
//! [libzbitmap]: https://github.com/eafer/libzbitmap

#![forbid(unsafe_code)]

pub mod decmpfs;
mod zbm;

pub use zbm::{decompress, Error};

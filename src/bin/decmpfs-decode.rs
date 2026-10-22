//! Rebuilds LZBITMAP-compressed files from extracted xattrs.
//!
//! Tools like 7-Zip extract a decmpfs-compressed file `X` as an empty file plus
//! its xattr `X:com.apple.decmpfs` and, for type 14, `X:com.apple.ResourceFork`.
//!
//! Usage: decmpfs-decode <X> <output>

use std::fs::{self, File};
use std::io::{BufReader, BufWriter};
use std::path::Path;
use std::process::ExitCode;

use zbitmap::decmpfs::{self, LZBITMAP_ATTR, LZBITMAP_RSRC};

fn run(input: &Path, output: &Path) -> Result<u64, Box<dyn std::error::Error>> {
    let side = |suffix: &str| {
        let mut s = input.as_os_str().to_owned();
        s.push(suffix);
        s
    };
    let xattr = fs::read(side(":com.apple.decmpfs"))?;
    let hdr = decmpfs::parse_header(&xattr)?;
    let out = BufWriter::new(File::create(output)?);
    let n = match hdr.algo {
        LZBITMAP_ATTR => decmpfs::decode_attr(&xattr, out)?,
        LZBITMAP_RSRC => {
            let rsrc = BufReader::new(File::open(side(":com.apple.ResourceFork"))?);
            decmpfs::decode_rsrc(hdr, rsrc, out)?
        }
        other => return Err(decmpfs::Error::Unsupported(other).into()),
    };
    Ok(n)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: {} <input> <output>", args[0]);
        return ExitCode::from(2);
    }
    let (input, output) = (Path::new(&args[1]), Path::new(&args[2]));
    match run(input, output) {
        Ok(n) => {
            println!("{} -> {} ({n} bytes)", input.display(), output.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{}: {e}", input.display());
            let _ = fs::remove_file(output);
            ExitCode::FAILURE
        }
    }
}

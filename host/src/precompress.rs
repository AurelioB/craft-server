//! Precompressed `.gz` and `.br` copies, written once at install time so the server never
//! compresses large WebAssembly modules per request.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::fsutil::{FILE_MODE, random_hex, walk_tree};

/// Responses smaller than this are not worth compressing.
pub const MIN_BYTES: u64 = 1024;
const COMPRESSIBLE: &[&str] = &[
    "wasm",
    "js",
    "mjs",
    "html",
    "css",
    "json",
    "svg",
    "webmanifest",
    "txt",
    "map",
    "xml",
];
const BROTLI_QUALITY: u32 = 9;
const BROTLI_WINDOW: u32 = 22;

pub fn is_compressible(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| COMPRESSIBLE.contains(&e))
}

fn write_atomically(
    dest: &Path,
    fill: impl FnOnce(&mut BufWriter<File>) -> io::Result<()>,
) -> io::Result<()> {
    let tmp = dest.with_file_name(format!(
        ".{}.{}.tmp",
        dest.file_name().unwrap().to_string_lossy(),
        random_hex(4)
    ));
    let result = (|| {
        let f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .open(&tmp)?;
        let mut w = BufWriter::new(f);
        fill(&mut w)?;
        let f = w.into_inner().map_err(|e| e.into_error())?;
        f.sync_all()?;
        fs::rename(&tmp, dest)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn gzip(src: &Path, dest: &Path) -> io::Result<()> {
    write_atomically(dest, |w| {
        let mut enc = flate2::write::GzEncoder::new(w, flate2::Compression::best());
        io::copy(&mut BufReader::new(File::open(src)?), &mut enc)?;
        enc.finish()?;
        Ok(())
    })
}

fn brotli(src: &Path, dest: &Path) -> io::Result<()> {
    write_atomically(dest, |w| {
        let mut enc = brotli::CompressorWriter::new(w, 1 << 16, BROTLI_QUALITY, BROTLI_WINDOW);
        io::copy(&mut BufReader::new(File::open(src)?), &mut enc)?;
        enc.flush()
    })
}

/// Add missing `.gz`/`.br` copies for compressible files of at least [`MIN_BYTES`].
/// Returns the number of files written.
pub fn precompress_tree(root: &Path) -> io::Result<usize> {
    let mut written = 0;
    for p in walk_tree(root)? {
        let meta = fs::symlink_metadata(&p)?;
        if !meta.is_file() || meta.len() < MIN_BYTES || !is_compressible(&p) {
            continue;
        }
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        let gz = p.with_file_name(format!("{name}.gz"));
        if !gz.exists() {
            gzip(&p, &gz)?;
            written += 1;
        }
        let br = p.with_file_name(format!("{name}.br"));
        if !br.exists() {
            brotli(&p, &br)?;
            written += 1;
        }
    }
    Ok(written)
}

/// Decoder for an existing precompressed copy, by suffix.
pub fn decoder(path: &Path) -> io::Result<Option<Box<dyn Read>>> {
    let f = File::open(path)?;
    let name = path.to_string_lossy();
    Ok(if name.ends_with(".gz") {
        Some(Box::new(flate2::read::MultiGzDecoder::new(f)))
    } else if name.ends_with(".br") {
        Some(Box::new(brotli::Decompressor::new(f, 1 << 16)))
    } else {
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_round_tripping_copies_only_where_useful() {
        let dir = tempfile::tempdir().unwrap();
        let body: Vec<u8> = b"export function f() { return 1; }\n".repeat(200);
        fs::write(dir.path().join("app.js"), &body).unwrap();
        fs::write(dir.path().join("tiny.js"), b"x").unwrap();
        fs::write(dir.path().join("photo.png"), vec![7u8; 4096]).unwrap();
        assert_eq!(precompress_tree(dir.path()).unwrap(), 2);
        for ext in ["gz", "br"] {
            let mut out = Vec::new();
            decoder(&dir.path().join(format!("app.js.{ext}")))
                .unwrap()
                .unwrap()
                .read_to_end(&mut out)
                .unwrap();
            assert_eq!(out, body, "{ext} copy decodes to the original");
        }
        assert!(!dir.path().join("tiny.js.gz").exists());
        assert!(!dir.path().join("photo.png.gz").exists());
        assert_eq!(
            precompress_tree(dir.path()).unwrap(),
            0,
            "existing copies are kept"
        );
    }
}

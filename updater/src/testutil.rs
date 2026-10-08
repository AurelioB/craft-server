//! Test support: a minimal zip writer giving exact control over names and unix modes, which
//! `zip::ZipWriter` sanitizes away (it cannot produce FIFOs, setuid bits or traversal names).

use std::io::Write;
use std::path::Path;

pub type Entry<'a> = (&'a str, Option<u32>, &'a [u8]);

/// Stored (uncompressed) archive. `mode` is the full st_mode written to the external attributes.
pub fn raw_zip(path: &Path, entries: &[Entry]) {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, mode, data) in entries {
        let mut crc = flate2::Crc::new();
        crc.update(data);
        let crc = crc.sum();
        let offset = out.len() as u32;
        let n = name.as_bytes();
        let local = |buf: &mut Vec<u8>| {
            buf.extend_from_slice(&20u16.to_le_bytes()); // version needed
            buf.extend_from_slice(&0u16.to_le_bytes()); // flags
            buf.extend_from_slice(&0u16.to_le_bytes()); // stored
            buf.extend_from_slice(&0u16.to_le_bytes()); // time
            buf.extend_from_slice(&0x5948u16.to_le_bytes()); // date 2024-10-08
            buf.extend_from_slice(&crc.to_le_bytes());
            buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
            buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
            buf.extend_from_slice(&(n.len() as u16).to_le_bytes());
            buf.extend_from_slice(&0u16.to_le_bytes()); // extra
        };
        out.extend_from_slice(&0x04034b50u32.to_le_bytes());
        local(&mut out);
        out.extend_from_slice(n);
        out.extend_from_slice(data);

        central.extend_from_slice(&0x02014b50u32.to_le_bytes());
        let made_by: u16 = if mode.is_some() { (3 << 8) | 20 } else { 20 };
        central.extend_from_slice(&made_by.to_le_bytes());
        local(&mut central);
        central.extend_from_slice(&0u16.to_le_bytes()); // comment
        central.extend_from_slice(&0u16.to_le_bytes()); // disk
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        central.extend_from_slice(&(mode.unwrap_or(0) << 16).to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(n);
    }
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x06054b50u32.to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    std::fs::write(path, out).unwrap();
}

/// Deflate-compressed archive written by the zip crate.
pub fn deflated_zip(path: &Path, entries: &[(&str, &[u8])]) {
    let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, data) in entries {
        w.start_file(*name, opts).unwrap();
        w.write_all(data).unwrap();
    }
    w.finish().unwrap();
}

/// A minimal valid web release: entry page, JS glue and a WebAssembly module.
pub fn site_entries(top: &str) -> Vec<(String, Vec<u8>)> {
    vec![
        (format!("{top}/index.html"), b"<html><script type=module>import init from './app.js'; init({module_or_path: './app_bg.wasm'});</script></html>".to_vec()),
        (format!("{top}/app.js"), b"export default function init() {}".to_vec()),
        (format!("{top}/app_bg.wasm"), b"\0asm\x01\0\0\0".to_vec()),
    ]
}

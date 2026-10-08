//! Validation of an extracted release before publication: static content checks, then a fetch
//! through the web server's private validation listener.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use sha2::{Digest, Sha256};

use crate::fsutil::walk_tree;
use crate::github::USER_AGENT;

const WASM_MAGIC: &[u8] = b"\0asm";
const NATIVE_MAGICS: &[&[u8]] = &[
    b"\x7fELF",
    b"\xfe\xed\xfa\xce",
    b"\xfe\xed\xfa\xcf",
    b"\xce\xfa\xed\xfe",
    b"\xcf\xfa\xed\xfe",
];
/// Files whose quoted asset references are checked against the release contents.
const SCANNED: &[&str] = &["index.html", "sw.js"];
const JS_TYPES: &[&str] = &["text/javascript", "application/javascript"];

static REF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"["'`]([^"'`\s<>()]+?\.(?:m?js|wasm|css|png|svg|ico|webmanifest|json|jpe?g|webp|gif|woff2?|ttf|otf))(?:\?[^"'`\s]*)?["'`]"#)
        .unwrap()
});

#[derive(Debug)]
pub enum ValidationError {
    /// The release cannot be served safely as published.
    Invalid(String),
    /// The private validation listener is unreachable; try again later.
    Unavailable(String),
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationError::Invalid(m) | ValidationError::Unavailable(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ValidationError {}

fn invalid<T>(msg: impl Into<String>) -> Result<T, ValidationError> {
    Err(ValidationError::Invalid(msg.into()))
}

#[derive(Debug, Default)]
pub struct Report {
    pub wasm_files: Vec<String>,
    pub js_files: Vec<String>,
    pub references: Vec<String>,
    pub warnings: Vec<String>,
}

fn head(path: &Path, n: usize) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(n);
    File::open(path)?.take(n as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

fn is_native_binary(path: &Path) -> io::Result<bool> {
    let h = head(path, 64)?;
    if NATIVE_MAGICS.iter().any(|m| h.starts_with(m)) {
        return Ok(true);
    }
    if h.starts_with(b"MZ") && h.len() >= 0x40 {
        let pe = u32::from_le_bytes(h[0x3c..0x40].try_into().unwrap()) as u64;
        let mut f = File::open(path)?;
        let mut sig = [0u8; 4];
        if io::Seek::seek(&mut f, io::SeekFrom::Start(pe)).is_ok() && f.read_exact(&mut sig).is_ok()
        {
            return Ok(&sig == b"PE\0\0");
        }
    }
    Ok(false)
}

fn sha256_reader(mut r: impl Read, limit: u64) -> io::Result<Option<[u8; 32]>> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut total = 0u64;
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            return Ok(Some(h.finalize().into()));
        }
        total += n as u64;
        if total > limit {
            return Ok(None);
        }
        h.update(&buf[..n]);
    }
}

/// The web server may answer with a precompressed `.gz` copy instead of the original, so the
/// copy must decompress to exactly the original bytes.
fn gzip_matches(gz: &Path, original: &Path, limit: u64) -> bool {
    let Ok(f) = File::open(gz) else { return false };
    let decoded = match sha256_reader(flate2::read::MultiGzDecoder::new(f), limit) {
        Ok(Some(d)) => d,
        _ => return false,
    };
    matches!(File::open(original).and_then(|f| sha256_reader(f, u64::MAX)), Ok(Some(d)) if d == decoded)
}

fn rel_string(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .into_owned()
}

/// Resolve `reference` (relative to `base_dir`) lexically, refusing to leave `root`.
fn resolve(root: &Path, base_dir: &Path, reference: &str) -> Option<PathBuf> {
    let mut parts: Vec<String> = base_dir
        .strip_prefix(root)
        .ok()?
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    for c in Path::new(reference).components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            _ => return None,
        }
    }
    Some(parts.iter().collect())
}

pub fn references(text: &str) -> Vec<String> {
    let mut refs: Vec<String> = REF_RE
        .captures_iter(text)
        .map(|c| c[1].to_string())
        .collect();
    refs.sort();
    refs.dedup();
    refs
}

/// Static checks on an extracted site rooted at `root`.
pub fn validate_tree(root: &Path, icon: &str, limit: u64) -> Result<Report, ValidationError> {
    let io_err = |e: io::Error| ValidationError::Invalid(format!("cannot inspect release: {e}"));
    let mut report = Report::default();
    if !root.join("index.html").is_file() {
        return invalid("entry page index.html is missing");
    }
    let mut paths = walk_tree(root).map_err(io_err)?;
    paths.sort();
    for p in &paths {
        let meta = fs::symlink_metadata(p).map_err(io_err)?;
        let rel = rel_string(root, p);
        if meta.file_type().is_symlink() {
            return invalid(format!("{rel}: symbolic links are not allowed"));
        }
        if !meta.is_file() {
            continue;
        }
        if is_native_binary(p).map_err(io_err)? {
            return invalid(format!(
                "{rel}: native executable or library in a web release (unexpected layout; if it is unused build output, exclude it in the app's manifest entry)"
            ));
        }
        if rel.ends_with(".wasm") {
            if head(p, 4).map_err(io_err)? != WASM_MAGIC {
                return invalid(format!("{rel}: not a WebAssembly module"));
            }
            report.wasm_files.push(rel.clone());
        } else if rel.ends_with(".js") || rel.ends_with(".mjs") {
            report.js_files.push(rel.clone());
        }
        if let Some(orig) = rel.strip_suffix(".gz") {
            let original = root.join(orig);
            if original.is_file() && !gzip_matches(p, &original, limit) {
                return invalid(format!("{rel}: precompressed copy does not match {orig}"));
            }
        }
    }
    if report.wasm_files.is_empty() {
        return invalid("no WebAssembly module found");
    }

    let mut scan: Vec<(PathBuf, Vec<String>)> = Vec::new();
    for name in SCANNED {
        let p = root.join(name);
        if p.is_file() {
            let text = fs::read_to_string(&p).unwrap_or_default();
            scan.push((p, references(&text)));
        }
    }
    let manifest = root.join("manifest.webmanifest");
    if manifest.is_file() {
        let v: serde_json::Value = serde_json::from_slice(&fs::read(&manifest).map_err(io_err)?)
            .map_err(|e| {
                ValidationError::Invalid(format!("manifest.webmanifest: invalid JSON: {e}"))
            })?;
        for key in ["start_url", "scope"] {
            if v.get(key)
                .and_then(|s| s.as_str())
                .is_some_and(|s| s.starts_with('/'))
            {
                return invalid(format!(
                    "manifest.webmanifest: {key} is root-absolute; the app needs path-prefix support"
                ));
            }
        }
        let icons = v
            .get("icons")
            .and_then(|i| i.as_array())
            .cloned()
            .unwrap_or_default();
        let srcs = icons
            .iter()
            .filter_map(|i| i.get("src")?.as_str().map(str::to_string))
            .collect();
        scan.push((manifest, srcs));
    }
    for (file, refs) in scan {
        let name = rel_string(root, &file);
        for r in refs {
            if r.starts_with("http://")
                || r.starts_with("https://")
                || r.starts_with("data:")
                || r.starts_with("blob:")
                || r.starts_with("//")
            {
                continue;
            }
            if r.starts_with('/') {
                return invalid(format!(
                    "{name} references {r:?} with a root-absolute URL; it cannot be served under a path prefix"
                ));
            }
            let Some(target) = resolve(root, file.parent().unwrap(), &r) else {
                return invalid(format!("{name} references {r:?} outside the release"));
            };
            if !root.join(&target).is_file() {
                return invalid(format!(
                    "{name} references {r:?}, which is not in the release"
                ));
            }
            report
                .references
                .push(target.to_string_lossy().into_owned());
        }
    }
    report.references.sort();
    report.references.dedup();
    if !icon.is_empty() && !root.join(icon).is_file() {
        report.warnings.push(format!(
            "configured icon {icon:?} not found; the launcher shows a monogram"
        ));
    }
    Ok(report)
}

fn encode_path(rel: &str) -> String {
    rel.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Fetch the staged candidate through the web server's private listener at `base_url` (which
/// maps to the staged site and ends with '/'). This exercises the real nginx configuration under
/// a nested path: MIME types, compression and reachability of referenced files.
pub fn check_serving(
    base_url: &str,
    report: &Report,
    timeout: Duration,
) -> Result<(), ValidationError> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .max_redirects(0)
        .user_agent(USER_AGENT)
        .build()
        .new_agent();
    let get = |rel: &str, gzip: bool| -> Result<(String, Option<String>), ValidationError> {
        let url = format!("{base_url}{}", encode_path(rel));
        let mut req = agent.get(&url);
        if gzip {
            req = req.header("Accept-Encoding", "gzip");
        }
        let mut resp = req.call().map_err(|e| {
            ValidationError::Unavailable(format!(
                "serving check: validation listener unreachable ({e})"
            ))
        })?;
        let status = resp.status().as_u16();
        if status != 200 {
            return invalid(format!(
                "serving check: {} returned HTTP {status}",
                if rel.is_empty() { "./" } else { rel }
            ));
        }
        let header = |k: &str| {
            resp.headers()
                .get(k)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let ctype = header("content-type")
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let encoding = header("content-encoding");
        let _ = resp
            .body_mut()
            .as_reader()
            .take(1 << 16)
            .read_to_end(&mut Vec::new());
        Ok((ctype, encoding))
    };
    let (ctype, _) = get("", false)?;
    if ctype != "text/html" {
        return invalid(format!("serving check: entry page served as {ctype}"));
    }
    for rel in &report.wasm_files {
        let (ctype, enc) = get(rel, true)?;
        if ctype != "application/wasm" {
            return invalid(format!(
                "serving check: {rel} served as {ctype}, expected application/wasm"
            ));
        }
        if enc.as_deref() != Some("gzip") {
            return invalid(format!("serving check: {rel} is not served compressed"));
        }
    }
    for rel in &report.js_files {
        let (ctype, _) = get(rel, false)?;
        if !JS_TYPES.contains(&ctype.as_str()) {
            return invalid(format!(
                "serving check: {rel} served as {ctype}, expected text/javascript"
            ));
        }
    }
    for rel in report
        .references
        .iter()
        .filter(|r| !report.wasm_files.contains(r) && !report.js_files.contains(r))
    {
        get(rel, false)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(files: &[(&str, &[u8])]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, data) in files {
            let p = dir.path().join(name);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, data).unwrap();
        }
        dir
    }

    const INDEX: &[u8] = b"<script type=module>import init from './app.js'; await init({ module_or_path: './app_bg.wasm' });</script>";

    #[test]
    fn valid_site_lists_assets_and_references() {
        let d = site(&[
            ("index.html", INDEX),
            ("app.js", b"x"),
            ("app_bg.wasm", b"\0asm\x01\0\0\0"),
        ]);
        let r = validate_tree(d.path(), "", 1 << 30).unwrap();
        assert_eq!(r.wasm_files, ["app_bg.wasm"]);
        assert_eq!(r.references, ["app.js", "app_bg.wasm"]);
    }

    #[test]
    fn root_absolute_and_missing_references_are_rejected() {
        let d = site(&[
            ("index.html", b"<script src=\"/app.js\"></script>"),
            ("app.js", b"x"),
            ("a.wasm", b"\0asm"),
        ]);
        let e = validate_tree(d.path(), "", 1 << 30)
            .unwrap_err()
            .to_string();
        assert!(e.contains("root-absolute"), "{e}");
        let d = site(&[
            ("index.html", b"<script src=\"./gone.js?v=1\"></script>"),
            ("a.wasm", b"\0asm"),
        ]);
        let e = validate_tree(d.path(), "", 1 << 30)
            .unwrap_err()
            .to_string();
        assert!(e.contains("not in the release"), "{e}");
        let d = site(&[
            ("index.html", b"<img src=\"../../x.png\">"),
            ("a.wasm", b"\0asm"),
        ]);
        let e = validate_tree(d.path(), "", 1 << 30)
            .unwrap_err()
            .to_string();
        assert!(e.contains("outside the release"), "{e}");
    }

    #[test]
    fn native_binaries_bad_wasm_and_mismatched_gzip_are_rejected() {
        let d = site(&[
            ("index.html", b"<html>"),
            ("a.wasm", b"\0asm"),
            ("deps/lib.so", b"\x7fELF\x02\x01"),
        ]);
        assert!(
            validate_tree(d.path(), "", 1 << 30)
                .unwrap_err()
                .to_string()
                .contains("native executable")
        );
        let d = site(&[("index.html", b"<html>"), ("a.wasm", b"<html>404</html>")]);
        assert!(
            validate_tree(d.path(), "", 1 << 30)
                .unwrap_err()
                .to_string()
                .contains("not a WebAssembly")
        );
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        io::Write::write_all(&mut gz, b"different").unwrap();
        let gz = gz.finish().unwrap();
        let d = site(&[
            ("index.html", b"<html>"),
            ("a.wasm", b"\0asm"),
            ("a.js", b"original"),
            ("a.js.gz", &gz),
        ]);
        assert!(
            validate_tree(d.path(), "", 1 << 30)
                .unwrap_err()
                .to_string()
                .contains("does not match")
        );
        let d = site(&[("index.html", b"<html>")]);
        assert!(
            validate_tree(d.path(), "", 1 << 30)
                .unwrap_err()
                .to_string()
                .contains("no WebAssembly")
        );
    }
}

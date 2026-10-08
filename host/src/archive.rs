//! Safe inspection and extraction of web-release zip archives.

use std::collections::BTreeMap;
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

use zip::ZipArchive;

use crate::config::Limits;
use crate::fsutil::{DIR_MODE, FILE_MODE};
use crate::glob::is_excluded;

const CHUNK: usize = 1 << 16;
/// Small files are exempt from the per-entry compression-ratio check.
const RATIO_MIN_BYTES: u64 = 1 << 20;

const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;

#[derive(Debug)]
pub struct UnsafeArchive(pub String);

impl std::fmt::Display for UnsafeArchive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UnsafeArchive {}

fn unsafe_err<T>(msg: impl Into<String>) -> Result<T, UnsafeArchive> {
    Err(UnsafeArchive(msg.into()))
}

#[derive(Debug, Clone)]
pub struct Member {
    pub index: usize,
    /// Path relative to the site root, '/'-separated.
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: Option<i64>,
}

#[derive(Debug)]
pub struct Plan {
    pub members: Vec<Member>,
    /// Stripped top-level directory; empty when the site is at the archive root.
    pub prefix: String,
    pub total_bytes: u64,
    pub excluded: Vec<String>,
}

pub fn open(path: &Path) -> Result<ZipArchive<File>, UnsafeArchive> {
    let file = File::open(path).map_err(|e| UnsafeArchive(format!("cannot open archive: {e}")))?;
    ZipArchive::new(file).map_err(|e| UnsafeArchive(format!("not a valid zip archive: {e}")))
}

fn clean_name(raw: &[u8]) -> Result<String, UnsafeArchive> {
    let Ok(name) = std::str::from_utf8(raw) else {
        return unsafe_err(format!(
            "entry {:?}: file names must be UTF-8",
            String::from_utf8_lossy(raw)
        ));
    };
    if name.contains('\0') || name.contains('\\') {
        return unsafe_err(format!(
            "entry {name:?}: backslashes and NUL bytes are not allowed"
        ));
    }
    let b = name.as_bytes();
    if name.starts_with('/') || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':') {
        return unsafe_err(format!("entry {name:?}: absolute paths are not allowed"));
    }
    let trimmed = name.trim_end_matches('/');
    if trimmed.is_empty()
        || trimmed
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return unsafe_err(format!(
            "entry {name:?}: path traversal or empty components are not allowed"
        ));
    }
    Ok(trimmed.to_string())
}

fn zip_mtime(dt: Option<zip::DateTime>) -> Option<i64> {
    let dt = dt?;
    let text = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    );
    crate::timeutil::parse_iso(&text)
}

pub fn plan_extraction(
    zip: &mut ZipArchive<File>,
    exclude: &[String],
    limits: &Limits,
) -> Result<Plan, UnsafeArchive> {
    if zip.len() as u64 > limits.max_files {
        return unsafe_err(format!(
            "archive has {} entries, above the limit of {}",
            zip.len(),
            limits.max_files
        ));
    }
    struct Raw {
        index: usize,
        name: String,
        is_dir: bool,
        size: u64,
        compressed: u64,
        mtime: Option<i64>,
    }
    let mut raws = Vec::with_capacity(zip.len());
    for index in 0..zip.len() {
        let f = zip
            .by_index_raw(index)
            .map_err(|e| UnsafeArchive(format!("entry {index}: {e}")))?;
        let name = clean_name(f.name_raw())?;
        let declared_dir = f.name_raw().ends_with(b"/");
        let is_dir = match f.unix_mode().map(|m| m & S_IFMT) {
            Some(S_IFLNK) => {
                return unsafe_err(format!("entry {name:?}: symbolic links are not allowed"));
            }
            Some(S_IFDIR) => true,
            Some(S_IFREG) | Some(0) | None => declared_dir,
            Some(_) => {
                return unsafe_err(format!(
                    "entry {name:?}: device, FIFO and socket entries are not allowed"
                ));
            }
        };
        if f.encrypted() {
            return unsafe_err(format!(
                "entry {name:?}: encrypted entries are not supported"
            ));
        }
        raws.push(Raw {
            index,
            name,
            is_dir,
            size: f.size(),
            compressed: f.compressed_size(),
            mtime: zip_mtime(f.last_modified()),
        });
    }

    // Layout: the official archives hold one top-level directory with the site in it.
    let mut tops: Vec<&str> = raws
        .iter()
        .map(|r| r.name.split('/').next().unwrap())
        .collect();
    tops.sort_unstable();
    tops.dedup();
    let prefix = match tops.as_slice() {
        [top]
            if raws
                .iter()
                .any(|r| r.name.contains('/') || (r.name == *top && r.is_dir)) =>
        {
            top.to_string()
        }
        _ => String::new(),
    };

    let mut members = Vec::new();
    let mut excluded = Vec::new();
    let mut seen: BTreeMap<String, bool> = BTreeMap::new();
    let mut folded: BTreeMap<String, String> = BTreeMap::new();
    let mut total = 0u64;
    for r in raws {
        let rel = if prefix.is_empty() {
            r.name.clone()
        } else if r.name == prefix {
            continue;
        } else {
            r.name[prefix.len() + 1..].to_string()
        };
        if is_excluded(&rel, exclude) {
            excluded.push(rel);
            continue;
        }
        if let Some(&was_dir) = seen.get(&rel) {
            if was_dir && r.is_dir {
                continue;
            }
            return unsafe_err(format!("entry {rel:?} appears more than once"));
        }
        let key = rel.to_lowercase();
        if let Some(other) = folded.get(&key) {
            return unsafe_err(format!("entries {other:?} and {rel:?} differ only in case"));
        }
        folded.insert(key, rel.clone());
        seen.insert(rel.clone(), r.is_dir);
        if !r.is_dir {
            total = total.saturating_add(r.size);
            if total > limits.max_extracted_bytes {
                return unsafe_err(format!(
                    "archive expands beyond the {}-byte limit",
                    limits.max_extracted_bytes
                ));
            }
            if r.size >= RATIO_MIN_BYTES
                && r.size
                    > limits
                        .max_compression_ratio
                        .saturating_mul(r.compressed.max(1))
            {
                return unsafe_err(format!("entry {rel:?} has a suspicious compression ratio"));
            }
        }
        members.push(Member {
            index: r.index,
            path: rel,
            is_dir: r.is_dir,
            size: r.size,
            mtime: r.mtime,
        });
    }
    for path in seen.keys() {
        let mut parent = Path::new(path).parent();
        while let Some(p) = parent.filter(|p| !p.as_os_str().is_empty()) {
            if seen.get(p.to_str().unwrap()) == Some(&false) {
                return unsafe_err(format!(
                    "entry {:?} is both a file and a directory",
                    p.display()
                ));
            }
            parent = p.parent();
        }
    }
    if seen.get("index.html") != Some(&false) {
        let place = if prefix.is_empty() {
            "at the archive root".to_string()
        } else {
            format!("inside {prefix}/")
        };
        return unsafe_err(format!("unexpected layout: no index.html {place}"));
    }
    Ok(Plan {
        members,
        prefix,
        total_bytes: total,
        excluded,
    })
}

/// Extract planned members into the empty directory `dest`; returns bytes written.
///
/// Files are created 0666 and directories 0777, both reduced by the umask: archive ownership,
/// setuid/setgid and executable bits are never restored.
pub fn extract(
    zip: &mut ZipArchive<File>,
    plan: &Plan,
    dest: &Path,
    limits: &Limits,
) -> anyhow::Result<u64> {
    let mut dirs = DirBuilder::new();
    dirs.recursive(true).mode(DIR_MODE);
    let mut written_total = 0u64;
    let mut buf = vec![0u8; CHUNK];
    let mut members: Vec<&Member> = plan.members.iter().collect();
    members.sort_by(|a, b| a.path.cmp(&b.path));
    for m in members {
        let target = dest.join(&m.path);
        if m.is_dir {
            dirs.create(&target)?;
            continue;
        }
        dirs.create(target.parent().unwrap())?;
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&target)?;
        let mut src = zip
            .by_index(m.index)
            .map_err(|e| UnsafeArchive(format!("entry {:?}: {e}", m.path)))?;
        let mut written = 0u64;
        loop {
            let n = src
                .read(&mut buf)
                .map_err(|e| UnsafeArchive(format!("entry {:?}: {e}", m.path)))?;
            if n == 0 {
                break;
            }
            written += n as u64;
            written_total += n as u64;
            if written > m.size || written_total > limits.max_extracted_bytes {
                return Err(UnsafeArchive(format!(
                    "entry {:?} expands beyond its declared size",
                    m.path
                ))
                .into());
            }
            out.write_all(&buf[..n])?;
        }
        if written != m.size {
            return Err(UnsafeArchive(format!(
                "entry {:?}: {written} bytes, declared {}",
                m.path, m.size
            ))
            .into());
        }
        out.sync_all()?;
        if let Some(t) = m.mtime {
            let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(t.max(0) as u64);
            let _ = out.set_modified(time);
        }
    }
    Ok(written_total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Entry, deflated_zip, raw_zip};

    fn plan_zip(p: &Path, exclude: &[&str], limits: &Limits) -> Result<Plan, UnsafeArchive> {
        let mut z = open(p)?;
        let ex: Vec<String> = exclude.iter().map(|s| s.to_string()).collect();
        plan_extraction(&mut z, &ex, limits)
    }

    fn plan_for(
        entries: &[Entry],
        exclude: &[&str],
        limits: &Limits,
    ) -> Result<Plan, UnsafeArchive> {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.zip");
        raw_zip(&p, entries);
        plan_zip(&p, exclude, limits)
    }

    #[test]
    fn official_layout_is_stripped_and_exclusions_skip_build_output() {
        let plan = plan_for(
            &[
                ("app-web-1.0.0/", None, b""),
                ("app-web-1.0.0/index.html", Some(0o100644), b"<html>"),
                ("app-web-1.0.0/app_bg.wasm", Some(0o100644), b"\0asm"),
                (
                    "app-web-1.0.0/build/x/build-script-build",
                    Some(0o100755),
                    b"\x7fELF",
                ),
            ],
            &["build/**"],
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(plan.prefix, "app-web-1.0.0");
        let paths: Vec<_> = plan.members.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, ["index.html", "app_bg.wasm"]);
        assert_eq!(plan.excluded, ["build/x/build-script-build"]);
    }

    #[test]
    fn traversal_absolute_and_backslash_names_are_rejected() {
        for bad in [
            "../evil.html",
            "a/../../evil",
            "/etc/passwd",
            "C:/x",
            "a\\b",
            "a//b",
        ] {
            let err = clean_name(bad.as_bytes()).unwrap_err();
            assert!(err.0.contains("not allowed"), "{bad}: {err}");
        }
    }

    #[test]
    fn symlinks_and_devices_are_rejected() {
        let err = plan_for(
            &[
                ("index.html", Some(0o100644), b"x"),
                ("link", Some(0o120777), b"/etc/passwd"),
            ],
            &[],
            &Limits::default(),
        )
        .unwrap_err();
        assert!(err.0.contains("symbolic links"), "{err}");
        let err = plan_for(
            &[
                ("index.html", Some(0o100644), b"x"),
                ("fifo", Some(0o010644), b""),
            ],
            &[],
            &Limits::default(),
        )
        .unwrap_err();
        assert!(err.0.contains("FIFO"), "{err}");
    }

    #[test]
    fn size_count_and_ratio_limits_are_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bomb.zip");
        let big = vec![0u8; 4 << 20];
        deflated_zip(&p, &[("index.html", b"x"), ("zeros.bin", &big)]);
        let limits = Limits {
            max_compression_ratio: 100,
            ..Limits::default()
        };
        let err = plan_zip(&p, &[], &limits).unwrap_err();
        assert!(err.0.contains("compression ratio"), "{err}");
        let limits = Limits {
            max_extracted_bytes: 1 << 20,
            ..Limits::default()
        };
        let err = plan_zip(&p, &[], &limits).unwrap_err();
        assert!(err.0.contains("expands beyond"), "{err}");
        let limits = Limits {
            max_files: 2,
            ..Limits::default()
        };
        let err = plan_for(
            &[
                ("index.html", None, b"x"),
                ("a", None, b"x"),
                ("b", None, b"x"),
            ],
            &[],
            &limits,
        )
        .unwrap_err();
        assert!(err.0.contains("entries, above the limit"), "{err}");
    }

    #[test]
    fn traversal_names_inside_an_archive_are_rejected_before_extraction() {
        let err = plan_for(
            &[
                ("index.html", None, b"x"),
                ("../../etc/cron.d/x", None, b"x"),
            ],
            &[],
            &Limits::default(),
        )
        .unwrap_err();
        assert!(err.0.contains("path traversal"), "{err}");
    }

    #[test]
    fn missing_entry_page_duplicates_and_case_collisions_are_rejected() {
        let err = plan_for(&[("site/app.js", None, b"x")], &[], &Limits::default()).unwrap_err();
        assert!(err.0.contains("no index.html inside site/"), "{err}");
        let err = plan_for(
            &[
                ("index.html", None, b"x"),
                ("A.js", None, b"x"),
                ("a.js", None, b"x"),
            ],
            &[],
            &Limits::default(),
        )
        .unwrap_err();
        assert!(err.0.contains("differ only in case"), "{err}");
        let err = plan_for(
            &[
                ("index.html", None, b"x"),
                ("a", None, b"x"),
                ("a/b", None, b"x"),
            ],
            &[],
            &Limits::default(),
        )
        .unwrap_err();
        assert!(err.0.contains("both a file and a directory"), "{err}");
    }

    #[test]
    fn extraction_never_restores_exec_or_setuid_bits() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.zip");
        raw_zip(
            &p,
            &[
                ("index.html", Some(0o104755), b"<html>"),
                ("sub/x.js", Some(0o100777), b"js"),
            ],
        );
        let mut z = open(&p).unwrap();
        let plan = plan_extraction(&mut z, &[], &Limits::default()).unwrap();
        let dest = dir.path().join("out");
        std::fs::create_dir(&dest).unwrap();
        let n = extract(&mut z, &plan, &dest, &Limits::default()).unwrap();
        assert_eq!(n, 8);
        for f in ["index.html", "sub/x.js"] {
            let mode = std::fs::metadata(dest.join(f))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o7111, 0, "{f} has mode {mode:o}");
        }
    }
}

//! Filesystem primitives: atomic writes, umask-respecting creation, read-only trees, symlink swaps.

use std::ffi::CString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Files are created 0666 and directories 0777; the process umask reduces both.
pub const FILE_MODE: u32 = 0o666;
pub const DIR_MODE: u32 = 0o777;

pub fn set_umask(mask: u32) -> u32 {
    // SAFETY: umask has no memory-safety preconditions.
    unsafe { libc::umask(mask as libc::mode_t) as u32 }
}

pub fn current_umask() -> u32 {
    let old = set_umask(0o022);
    set_umask(old);
    old
}

pub fn parse_umask(text: &str) -> Result<u32> {
    let t = text.trim();
    if t.is_empty() || t.len() > 4 || !t.bytes().all(|c| (b'0'..=b'7').contains(&c)) {
        bail!("FILE_UMASK must be an octal value such as 0002 or 0022, got {t:?}");
    }
    let mask = u32::from_str_radix(t, 8)?;
    if mask & 0o700 != 0 {
        bail!("FILE_UMASK {t} removes owner permissions; the updater needs owner read/write");
    }
    Ok(mask)
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    if File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_err()
    {
        // Uniqueness, not secrecy, is what callers need.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = std::process::id() as u64
            ^ COUNTER.fetch_add(1, Ordering::Relaxed)
            ^ crate::timeutil::now_epoch() as u64;
        buf.iter_mut()
            .enumerate()
            .for_each(|(i, b)| *b = (n >> ((i % 8) * 8)) as u8);
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn fsync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

pub fn make_dirs(path: &Path) -> io::Result<()> {
    // The kernel copies a parent's setgid bit to new subdirectories.
    DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(path)
}

pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().context("path has no parent")?;
    make_dirs(dir).with_context(|| format!("create {}", dir.display()))?;
    let name = path
        .file_name()
        .context("path has no file name")?
        .to_string_lossy();
    let tmp = dir.join(format!(".{name}.{}.tmp", random_hex(6)));
    let result = (|| -> io::Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        fsync_dir(dir)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("write {}", path.display()))
}

pub fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut data = serde_json::to_vec_pretty(value)?;
    data.push(b'\n');
    atomic_write(path, &data)
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(data) => Ok(Some(
            serde_json::from_slice(&data).with_context(|| format!("parse {}", path.display()))?,
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn walk(root: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_dir() {
            walk(&entry.path(), out)?;
        }
        out.push(entry.path());
    }
    Ok(())
}

/// Every path below `root` (children before their directory), excluding `root`.
pub fn walk_tree(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk(root, &mut out)?;
    Ok(out)
}

/// Remove write permission from a published tree; setgid and other bits are preserved.
pub fn strip_write_bits(root: &Path) -> io::Result<()> {
    let mut paths = walk_tree(root)?;
    paths.push(root.to_path_buf());
    for p in paths {
        let meta = fs::symlink_metadata(&p)?;
        if meta.file_type().is_symlink() {
            continue;
        }
        let mode = meta.permissions().mode() & 0o7777;
        fs::set_permissions(&p, fs::Permissions::from_mode(mode & !0o222))?;
    }
    Ok(())
}

fn restore_owner_write(root: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(root)?;
    if !meta.is_dir() {
        return Ok(());
    }
    let mode = meta.permissions().mode() & 0o7777;
    if mode & 0o200 == 0 {
        fs::set_permissions(root, fs::Permissions::from_mode(mode | 0o200))?;
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            restore_owner_write(&entry.path())?;
        }
    }
    Ok(())
}

/// Remove a file, symlink or (possibly read-only) directory tree. Missing paths are fine.
pub fn remove_tree(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(meta) if meta.is_dir() => {
            restore_owner_write(path)?;
            fs::remove_dir_all(path)
        }
        Ok(_) => fs::remove_file(path),
    }
}

/// Atomically point `link` at `target` (a relative path).
pub fn replace_symlink(link: &Path, target: &str) -> io::Result<()> {
    let name = link
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = link.with_file_name(format!(".{name}.{}.tmp", random_hex(6)));
    std::os::unix::fs::symlink(target, &tmp)?;
    if let Err(e) = fs::rename(&tmp, link) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fsync_dir(link.parent().unwrap_or(Path::new(".")))
}

pub fn free_bytes(path: &Path) -> io::Result<u64> {
    let c = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` a valid out-pointer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st.f_bavail as u64 * st.f_frsize as u64)
}

pub fn tree_size(root: &Path) -> u64 {
    walk_tree(root)
        .unwrap_or_default()
        .iter()
        .filter_map(|p| fs::symlink_metadata(p).ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

/// Move a file, copying when source and destination are on different filesystems.
pub fn move_file(src: &Path, dest: &Path) -> Result<()> {
    match fs::rename(src, dest) {
        Ok(()) => return Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {}
        Err(e) => {
            return Err(e).with_context(|| format!("move {} to {}", src.display(), dest.display()));
        }
    }
    let dir = dest.parent().context("destination has no parent")?;
    let tmp = dir.join(format!(".{}.tmp", random_hex(6)));
    let result = (|| -> io::Result<()> {
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .open(&tmp)?;
        io::copy(&mut File::open(src)?, &mut out)?;
        out.sync_all()?;
        fs::rename(&tmp, dest)?;
        fsync_dir(dir)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("copy {} to {}", src.display(), dest.display()))?;
    fs::remove_file(src).ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn umask_parsing_rejects_owner_bits_and_garbage() {
        assert_eq!(parse_umask("0002").unwrap(), 0o002);
        assert_eq!(parse_umask("027").unwrap(), 0o027);
        assert!(parse_umask("0700").is_err());
        assert!(parse_umask("8").is_err());
        assert!(parse_umask("").is_err());
    }

    #[test]
    fn read_only_tree_keeps_setgid_and_can_be_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("rel");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/a.js"), b"x").unwrap();
        fs::set_permissions(root.join("sub"), fs::Permissions::from_mode(0o2775)).unwrap();
        let had_setgid = fs::metadata(root.join("sub")).unwrap().mode() & 0o2000 != 0;
        strip_write_bits(&root).unwrap();
        let sub = fs::metadata(root.join("sub")).unwrap().mode();
        assert_eq!(sub & 0o222, 0);
        assert_eq!(sub & 0o2000 != 0, had_setgid);
        assert_eq!(
            fs::metadata(root.join("sub/a.js")).unwrap().mode() & 0o222,
            0
        );
        remove_tree(&root).unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn symlink_replacement_is_atomic_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("current");
        replace_symlink(&link, "1.0.0").unwrap();
        replace_symlink(&link, "2.0.0").unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), PathBuf::from("2.0.0"));
        assert_eq!(
            fs::read_dir(tmp.path()).unwrap().count(),
            1,
            "no temporary links left behind"
        );
    }
}

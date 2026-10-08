//! Persistent data layout, atomic release promotion and crash reconciliation.
//!
//! ```text
//! DATA_DIR/
//!   public/                        served by the web server: status.json, per-app entry links
//!     <entry> -> ../<release_dir>  relative symlink for each enabled app
//!   <release_dir>/                 e.g. releases/photocraft
//!     <version>/                   immutable release, contains .craft-release.json
//!     current -> <version>         active-version pointer, replaced atomically
//!   .staging/                      unpublished extraction and validation (same filesystem)
//! ```

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::{AppConfig, Config};
use crate::fsutil::{
    atomic_write_json, fsync_dir, make_dirs, random_hex, read_json, remove_tree, replace_symlink,
};
use crate::store::{InstalledRelease, Store};

pub const MARKER: &str = ".craft-release.json";
pub const CURRENT: &str = "current";

pub fn valid_release_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || b"._+-".contains(c))
        && name != CURRENT
}

fn relative_path(from_dir: &Path, to: &Path) -> String {
    let from: Vec<_> = from_dir.components().collect();
    let to: Vec<_> = to.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".into(); from.len() - common];
    parts.extend(
        to[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    parts.join("/")
}

pub fn ensure_base(cfg: &Config) -> Result<()> {
    for p in [cfg.paths.public(), cfg.paths.staging()] {
        make_dirs(&p).with_context(|| format!("create {}", p.display()))?;
    }
    Ok(())
}

pub fn ensure_app(cfg: &Config, app: &AppConfig) -> Result<PathBuf> {
    let root = cfg.release_root(app);
    make_dirs(&root).with_context(|| format!("create {}", root.display()))?;
    let (a, b) = (
        fs::metadata(&root)?.dev(),
        fs::metadata(cfg.paths.staging())?.dev(),
    );
    if a != b {
        bail!(
            "{} is on a different filesystem than DATA_DIR/.staging; atomic promotion needs both on the same filesystem",
            app.release_dir
        );
    }
    Ok(root)
}

/// Point public/<entry> at each enabled app's release root; remove links of disabled apps.
pub fn sync_public_links(cfg: &Config) -> Result<()> {
    let public = cfg.paths.public();
    let wanted: Vec<(String, String)> = cfg
        .enabled_apps()
        .map(|a| {
            (
                a.entry.clone(),
                relative_path(&public, &cfg.release_root(a)),
            )
        })
        .collect();
    for entry in fs::read_dir(&public)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // Only symlinks are ours; regular files and directories are never removed.
        if entry.file_type()?.is_symlink() && !wanted.iter().any(|(e, _)| *e == name) {
            fs::remove_file(entry.path())?;
        }
    }
    for (entry, target) in wanted {
        let link = public.join(&entry);
        match fs::symlink_metadata(&link) {
            Ok(m) if m.file_type().is_symlink() => {
                if fs::read_link(&link)?.to_string_lossy() == target {
                    continue;
                }
            }
            Ok(_) => bail!(
                "{} exists and is not a symlink; move it away so the app can be published",
                link.display()
            ),
            Err(_) => {}
        }
        replace_symlink(&link, &target)?;
    }
    Ok(())
}

pub fn active_version(cfg: &Config, app: &AppConfig) -> Option<String> {
    let root = cfg.release_root(app);
    let target = fs::read_link(root.join(CURRENT)).ok()?;
    let target = target.to_str()?;
    (valid_release_name(target) && root.join(target).join(MARKER).is_file())
        .then(|| target.to_string())
}

/// Release directories on disk, with their marker record (None when the marker is missing).
pub fn installed_dirs(cfg: &Config, app: &AppConfig) -> Vec<(String, Option<InstalledRelease>)> {
    let root = cfg.release_root(app);
    let Ok(entries) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if name == CURRENT || name.starts_with('.') || !is_dir {
            continue;
        }
        out.push((name, read_json(&e.path().join(MARKER)).ok().flatten()));
    }
    out.sort_by(|a, b| crate::version::cmp_text(&a.0, &b.0));
    out
}

pub fn new_staging(cfg: &Config, app: &AppConfig, version: &str) -> Result<PathBuf> {
    let p = cfg
        .paths
        .staging()
        .join(format!("{}-{version}-{}", app.id, random_hex(6)));
    fs::create_dir(&p).with_context(|| format!("create {}", p.display()))?;
    Ok(p)
}

pub fn write_marker(site: &Path, record: &InstalledRelease) -> Result<()> {
    atomic_write_json(&site.join(MARKER), record)
}

/// Move a validated site into place: one rename on one filesystem, so a release directory is
/// either absent or complete.
pub fn promote(cfg: &Config, app: &AppConfig, site: &Path, version: &str) -> Result<PathBuf> {
    if !valid_release_name(version) {
        bail!("refusing release directory name {version:?}");
    }
    let root = ensure_app(cfg, app)?;
    let dest = root.join(version);
    if fs::symlink_metadata(&dest).is_ok() {
        bail!(
            "{}/{version} already exists; releases are never overwritten",
            app.release_dir
        );
    }
    fs::rename(site, &dest).with_context(|| format!("publish {}", dest.display()))?;
    fsync_dir(&root)?;
    if let Some(parent) = site.parent() {
        fsync_dir(parent)?;
    }
    Ok(dest)
}

pub fn activate(cfg: &Config, app: &AppConfig, version: &str) -> Result<()> {
    let root = cfg.release_root(app);
    if !valid_release_name(version) || !root.join(version).join(MARKER).is_file() {
        bail!("{} {version} is not an installed release", app.id);
    }
    replace_symlink(&root.join(CURRENT), version)
        .with_context(|| format!("activate {} {version}", app.id))
}

/// Atomically unpublish a release; returns the moved path to delete afterwards.
pub fn trash(cfg: &Config, app: &AppConfig, version: &str) -> Result<Option<PathBuf>> {
    let root = cfg.release_root(app);
    let src = root.join(version);
    let Ok(meta) = fs::symlink_metadata(&src) else {
        return Ok(None);
    };
    // Moving a directory to a new parent needs write permission on it (to update "..").
    if meta.is_dir() {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o7777;
        fs::set_permissions(&src, fs::Permissions::from_mode(mode | 0o200))?;
    }
    let dest = cfg
        .paths
        .staging()
        .join(format!("trash-{}-{version}-{}", app.id, random_hex(6)));
    fs::rename(&src, &dest).with_context(|| format!("unpublish {}", src.display()))?;
    fsync_dir(&root)?;
    Ok(Some(dest))
}

/// Remove leftovers of interrupted operations. The caller holds the operation lock.
pub fn clean_staging(cfg: &Config) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    for dir in [cfg.paths.staging(), cfg.paths.work.join("downloads")] {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            remove_tree(&e.path()).with_context(|| format!("remove {}", e.path().display()))?;
            removed.push(e.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(removed)
}

/// Bring state records in line with what is actually published, e.g. after a crash.
///
/// The `current` symlink is the source of truth for the active release: it is replaced
/// atomically, so it always names a complete release or nothing.
pub fn reconcile(cfg: &Config, store: &Store) -> Result<Vec<String>> {
    let mut notes: Vec<String> = clean_staging(cfg)?
        .into_iter()
        .map(|n| format!("removed interrupted work: {n}"))
        .collect();
    for app in &cfg.apps {
        let root = cfg.release_root(app);
        if !root.is_dir() {
            continue;
        }
        let mut state = store.load(&app.id)?;
        let before = state.clone();
        let on_disk = installed_dirs(cfg, app);
        for (version, marker) in &on_disk {
            match marker {
                None => notes.push(format!(
                    "{}: {}/{version} has no release marker; left untouched and not served",
                    app.id, app.release_dir
                )),
                Some(m) if !state.installed.contains_key(version) => {
                    state.installed.insert(version.clone(), m.clone());
                    notes.push(format!(
                        "{}: recorded release {version} found on disk",
                        app.id
                    ));
                }
                Some(_) => {}
            }
        }
        let present: Vec<&String> = on_disk
            .iter()
            .filter(|(_, m)| m.is_some())
            .map(|(v, _)| v)
            .collect();
        state.installed.retain(|v, _| {
            let keep = present.contains(&v);
            if !keep {
                notes.push(format!("{}: dropped record of missing release {v}", app.id));
            }
            keep
        });
        let link = root.join(CURRENT);
        let mut active = active_version(cfg, app);
        if active.is_none() && fs::symlink_metadata(&link).is_ok() {
            fs::remove_file(&link)?;
            notes.push(format!(
                "{}: removed dangling active-version pointer",
                app.id
            ));
        }
        if active.is_none()
            && let Some((v, _)) = state
                .installed
                .iter()
                .max_by(|a, b| a.1.installed_at.cmp(&b.1.installed_at))
        {
            let v = v.clone();
            activate(cfg, app, &v)?;
            notes.push(format!(
                "{}: activated {v}, the most recently installed release",
                app.id
            ));
            active = Some(v);
        }
        state.active = active;
        if state != before {
            store.save(&app.id, &state)?;
        }
    }
    for n in &notes {
        log::info!("reconcile: {n}");
    }
    Ok(notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_links_climb_out_of_public() {
        assert_eq!(
            relative_path(
                Path::new("/srv/data/public"),
                Path::new("/srv/data/releases/photocraft")
            ),
            "../releases/photocraft"
        );
        assert_eq!(
            relative_path(Path::new("/d/public"), Path::new("/d/a b/c")),
            "../a b/c"
        );
    }

    #[test]
    fn release_names_exclude_pointer_and_hidden_names() {
        assert!(valid_release_name("0.5.0"));
        assert!(valid_release_name("0.1.1-rc.5"));
        assert!(!valid_release_name("current"));
        assert!(!valid_release_name(".staging"));
        assert!(!valid_release_name("../x"));
    }
}

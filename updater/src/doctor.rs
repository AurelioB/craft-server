//! `doctor`: configuration, identity, permission, space and filesystem checks with actionable
//! preparation instructions. Never changes ownership or permissions itself.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{Config, ConfigError, Paths, load_config};
use crate::fsutil::{current_umask, free_bytes, parse_umask, random_hex};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

pub struct Finding {
    pub level: Level,
    pub what: String,
    pub fix: Option<String>,
}

#[derive(Default)]
pub struct Report {
    pub findings: Vec<Finding>,
}

impl Report {
    fn ok(&mut self, what: impl Into<String>) {
        self.findings.push(Finding {
            level: Level::Ok,
            what: what.into(),
            fix: None,
        });
    }
    fn warn(&mut self, what: impl Into<String>, fix: Option<String>) {
        self.findings.push(Finding {
            level: Level::Warn,
            what: what.into(),
            fix,
        });
    }
    fn fail(&mut self, what: impl Into<String>, fix: Option<String>) {
        self.findings.push(Finding {
            level: Level::Fail,
            what: what.into(),
            fix,
        });
    }

    pub fn worst(&self) -> Level {
        self.findings
            .iter()
            .map(|f| f.level)
            .max()
            .unwrap_or(Level::Ok)
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        for f in &self.findings {
            let tag = match f.level {
                Level::Ok => "ok  ",
                Level::Warn => "WARN",
                Level::Fail => "FAIL",
            };
            out.push_str(&format!("[{tag}] {}\n", f.what));
            if let Some(fix) = &f.fix {
                for line in fix.lines() {
                    out.push_str(&format!("       {line}\n"));
                }
            }
        }
        out
    }
}

/// A numeric identity: uid, primary gid and supplemental groups.
#[derive(Debug, Clone)]
pub struct Identity {
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
}

impl Identity {
    pub fn current() -> Self {
        // SAFETY: plain libc identity queries.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        let n = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        let mut groups = vec![0 as libc::gid_t; n.max(0) as usize];
        let n = unsafe { libc::getgroups(groups.len() as i32, groups.as_mut_ptr()) };
        groups.truncate(n.max(0) as usize);
        Self { uid, gid, groups }
    }

    pub fn in_group(&self, gid: u32) -> bool {
        self.gid == gid || self.groups.contains(&gid)
    }

    /// Permission bits (rwx as 0o7) this identity gets on a file with the given owner and mode.
    pub fn access(&self, uid: u32, gid: u32, mode: u32) -> u32 {
        if self.uid == 0 {
            return 0o7;
        }
        if self.uid == uid {
            (mode >> 6) & 0o7
        } else if self.in_group(gid) {
            (mode >> 3) & 0o7
        } else {
            mode & 0o7
        }
    }
}

fn parse_id(name: &str) -> Result<Option<u32>, String> {
    match std::env::var(name) {
        Err(_) => Ok(None),
        Ok(v) if v.trim().is_empty() => Ok(None),
        Ok(v) => v
            .trim()
            .parse::<u32>()
            .map(Some)
            .map_err(|_| format!("{name}={v:?} is not a numeric id")),
    }
}

/// Web-server identity as configured in Compose, passed to the updater for compatibility checks.
pub fn web_identity() -> Result<Option<Identity>, String> {
    let uid = parse_id("CRAFT_WEB_UID")?;
    let gid = parse_id("CRAFT_WEB_GID")?;
    let groups = match std::env::var("CRAFT_WEB_GROUPS") {
        Ok(v) => v
            .split([',', ' '])
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.parse::<u32>()
                    .map_err(|_| format!("CRAFT_WEB_GROUPS contains non-numeric {s:?}"))
            })
            .collect::<Result<Vec<_>, _>>()?,
        Err(_) => Vec::new(),
    };
    Ok(match (uid, gid) {
        (Some(uid), Some(gid)) => Some(Identity { uid, gid, groups }),
        _ => None,
    })
}

fn host_path(label: &str) -> Option<String> {
    std::env::var(format!("CRAFT_HOST_{label}"))
        .ok()
        .filter(|v| !v.trim().is_empty())
}

fn prepare_hint(label: &str, writable_by: &Identity, umask: u32) -> String {
    let host = host_path(label).unwrap_or_else(|| format!("<{label} on the host>"));
    let mode = if umask & 0o020 == 0 { "2775" } else { "2755" };
    format!(
        "prepare on the host (once, as an administrator):\n  sudo install -d -o {} -g {} -m {mode} \"{host}\"",
        writable_by.uid, writable_by.gid
    )
}

fn probe_write(dir: &Path) -> Result<(), std::io::Error> {
    let p = dir.join(format!(".doctor-{}", random_hex(4)));
    fs::write(&p, b"probe")?;
    fs::remove_file(&p)
}

fn describe(meta: &fs::Metadata) -> String {
    format!(
        "owner {}:{} mode {:04o}",
        meta.uid(),
        meta.gid(),
        meta.permissions().mode() & 0o7777
    )
}

fn check_dir(
    r: &mut Report,
    label: &str,
    path: &Path,
    writable: bool,
    me: &Identity,
    umask: u32,
) -> Option<fs::Metadata> {
    let meta = match fs::metadata(path) {
        Ok(m) if m.is_dir() => m,
        Ok(_) => {
            r.fail(
                format!("{label} ({}) is not a directory", path.display()),
                None,
            );
            return None;
        }
        Err(e) => {
            r.fail(
                format!("{label} ({}) is not accessible: {e}", path.display()),
                Some(prepare_hint(label, me, umask)),
            );
            return None;
        }
    };
    if fs::read_dir(path).is_err() {
        r.fail(
            format!(
                "{label} ({}) is not readable by uid {} ({})",
                path.display(),
                me.uid,
                describe(&meta)
            ),
            Some(prepare_hint(label, me, umask)),
        );
        return Some(meta);
    }
    if writable {
        match probe_write(path) {
            Ok(()) => {
                let setgid = if meta.permissions().mode() & 0o2000 != 0 {
                    ", setgid: new files inherit the group"
                } else {
                    ""
                };
                r.ok(format!(
                    "{label} ({}) is writable by {}:{} ({}{setgid})",
                    path.display(),
                    me.uid,
                    me.gid,
                    describe(&meta)
                ));
            }
            Err(e) => r.fail(
                format!(
                    "{label} ({}) is not writable by uid {} gid {} ({}): {e}",
                    path.display(),
                    me.uid,
                    me.gid,
                    describe(&meta)
                ),
                Some(prepare_hint(label, me, umask)),
            ),
        }
    } else {
        r.ok(format!(
            "{label} ({}) is readable ({})",
            path.display(),
            describe(&meta)
        ));
    }
    Some(meta)
}

/// Can `web` read files the updater creates in `dir` (given umask and setgid inheritance)?
fn new_file_access(
    web: &Identity,
    me: &Identity,
    dir_meta: &fs::Metadata,
    umask: u32,
) -> (bool, String) {
    let group = if dir_meta.permissions().mode() & 0o2000 != 0 {
        dir_meta.gid()
    } else {
        me.gid
    };
    let file_mode = 0o666 & !umask;
    let dir_mode = 0o777 & !umask;
    let file_ok = web.access(me.uid, group, file_mode) & 0o4 != 0;
    let dir_ok = web.access(me.uid, group, dir_mode) & 0o5 == 0o5;
    (
        file_ok && dir_ok,
        format!(
            "new files {}:{group} mode {file_mode:04o}, directories {dir_mode:04o}",
            me.uid
        ),
    )
}

fn walk_unreadable(root: &Path, web: &Identity, limit: usize, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        if out.len() >= limit {
            return;
        }
        let Ok(m) = e.metadata() else { continue };
        if e.file_name().to_string_lossy().starts_with(".staging") {
            continue;
        }
        let need = if m.is_dir() { 0o5 } else { 0o4 };
        if web.access(m.uid(), m.gid(), m.permissions().mode()) & need != need {
            out.push(format!("{} ({})", e.path().display(), describe(&m)));
        } else if m.is_dir() {
            walk_unreadable(&e.path(), web, limit, out);
        }
    }
}

fn check_host_paths(r: &mut Report) {
    let labels = [
        "CONFIG_DIR",
        "DATA_DIR",
        "STATE_DIR",
        "CACHE_DIR",
        "WORK_DIR",
        "LOG_DIR",
    ];
    let paths: Vec<(&str, PathBuf)> = labels
        .iter()
        .filter_map(|l| host_path(l).map(|p| (*l, PathBuf::from(p.trim_end_matches('/')))))
        .collect();
    let mut bad = false;
    for (i, (a, pa)) in paths.iter().enumerate() {
        for (b, pb) in &paths[i + 1..] {
            if pa.starts_with(pb) || pb.starts_with(pa) {
                r.fail(
                    format!(
                        "host paths {a} ({}) and {b} ({}) overlap; use separate directories",
                        pa.display(),
                        pb.display()
                    ),
                    None,
                );
                bad = true;
            }
        }
    }
    if !paths.is_empty() && !bad {
        r.ok("host directories do not overlap");
    }
}

fn check_same_inodes(r: &mut Report, paths: &Paths) {
    let mut roots = vec![
        ("DATA_DIR", paths.data.clone()),
        ("STATE_DIR", paths.state.clone()),
        ("CACHE_DIR", paths.cache.clone()),
        ("WORK_DIR", paths.work.clone()),
        ("CONFIG_DIR", paths.config_dir()),
    ];
    if let Some(l) = &paths.logs {
        roots.push(("LOG_DIR", l.clone()));
    }
    let ids: Vec<_> = roots
        .iter()
        .filter_map(|(l, p)| fs::metadata(p).ok().map(|m| (*l, m.dev(), m.ino())))
        .collect();
    for (i, a) in ids.iter().enumerate() {
        for b in &ids[i + 1..] {
            if a.1 == b.1 && a.2 == b.2 {
                r.fail(
                    format!(
                        "{} and {} are the same host directory; they must be separate",
                        a.0, b.0
                    ),
                    None,
                );
            }
        }
    }
}

pub fn run(paths: Paths, network: bool) -> Report {
    let mut r = Report::default();
    let me = Identity::current();

    // Identity and umask.
    if me.uid == 0 {
        r.fail("the updater runs as root; set RUN_UID/RUN_GID (or UPDATER_UID/UPDATER_GID) to a non-root identity", None);
    } else {
        r.ok(format!(
            "updater runs as uid {} gid {} groups {:?}",
            me.uid, me.gid, me.groups
        ));
    }
    let umask = match std::env::var("FILE_UMASK")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        Some(v) => match parse_umask(&v) {
            Ok(m) => {
                r.ok(format!("FILE_UMASK {m:04o}"));
                m
            }
            Err(e) => {
                r.fail(format!("{e}"), None);
                current_umask()
            }
        },
        None => {
            r.warn(
                format!(
                    "FILE_UMASK is not set; using the process umask {:04o}",
                    current_umask()
                ),
                None,
            );
            current_umask()
        }
    };
    for var in [
        "RUN_UID",
        "RUN_GID",
        "CRAFT_WEB_UID",
        "CRAFT_WEB_GID",
        "CRAFT_UPDATER_UID",
        "CRAFT_UPDATER_GID",
    ] {
        match parse_id(var) {
            Err(e) => r.fail(e, None),
            Ok(Some(0)) => r.warn(
                format!(
                    "{var}=0 configures root; non-root operation is the default and recommended"
                ),
                None,
            ),
            _ => {}
        }
    }
    if let (Ok(Some(uid)), Ok(Some(gid))) =
        (parse_id("CRAFT_UPDATER_UID"), parse_id("CRAFT_UPDATER_GID"))
        && (uid != me.uid || gid != me.gid)
    {
        r.warn(format!("configured updater identity {uid}:{gid} differs from the running identity {}:{}", me.uid, me.gid), Some("user-namespace remapping or Docker Desktop file sharing can change effective ownership; see docs/permissions.md".into()));
    }

    // Configuration.
    let cfg: Option<Config> = match load_config(paths.clone()) {
        Ok(c) => {
            if c.config_file_found {
                r.ok(format!(
                    "configuration {} is valid ({} apps, {} enabled)",
                    paths.config_file.display(),
                    c.apps.len(),
                    c.enabled_apps().count()
                ));
            } else {
                r.warn(
                    format!(
                        "{} not found; using built-in defaults",
                        paths.config_file.display()
                    ),
                    Some("copy config.example.toml to CONFIG_DIR/config.toml to customise".into()),
                );
            }
            Some(c)
        }
        Err(ConfigError(problems)) => {
            for p in problems {
                r.fail(format!("config: {p}"), None);
            }
            None
        }
    };

    check_host_paths(&mut r);
    check_same_inodes(&mut r, &paths);

    // Directories.
    check_dir(&mut r, "CONFIG_DIR", &paths.config_dir(), false, &me, umask);
    if probe_write(&paths.config_dir()).is_ok() {
        r.warn(
            "CONFIG_DIR is writable by the updater; mount it read-only",
            None,
        );
    }
    let data_meta = check_dir(&mut r, "DATA_DIR", &paths.data, true, &me, umask);
    for (label, p) in [
        ("STATE_DIR", &paths.state),
        ("CACHE_DIR", &paths.cache),
        ("WORK_DIR", &paths.work),
    ] {
        check_dir(&mut r, label, p, true, &me, umask);
    }
    if let Some(l) = &paths.logs {
        check_dir(&mut r, "LOG_DIR", l, true, &me, umask);
    }

    if let Some(cfg) = &cfg {
        // Space.
        for (label, p, need) in [
            (
                "DATA_DIR",
                &paths.data,
                cfg.limits.max_extracted_bytes / 4 + cfg.limits.min_free_bytes,
            ),
            ("WORK_DIR", &paths.work, cfg.limits.min_free_bytes),
            ("CACHE_DIR", &paths.cache, cfg.limits.min_free_bytes),
        ] {
            match free_bytes(p) {
                Ok(free) if free >= need => r.ok(format!("{label} has {} MiB free", free >> 20)),
                Ok(free) => r.warn(format!("{label} has only {} MiB free; updates need room for downloads and two releases side by side", free >> 20), None),
                Err(_) => {}
            }
        }
        // Atomic promotion: staging and every release directory on one filesystem.
        if let Ok(dm) = fs::metadata(&paths.data) {
            let mut ok = true;
            for app in &cfg.apps {
                let root = cfg.release_root(app);
                if let Ok(m) = fs::metadata(&root)
                    && m.dev() != dm.dev()
                {
                    ok = false;
                    r.fail(format!("{} ({}) is a separate filesystem from DATA_DIR; atomic promotion requires one filesystem", app.release_dir, app.id), None);
                }
            }
            if ok {
                r.ok("release directories share DATA_DIR's filesystem (atomic promotion possible)");
            }
        }
        // Credentials.
        if let Some(tp) = cfg.token_path() {
            match fs::metadata(&tp) {
                Ok(m) => {
                    if m.permissions().mode() & 0o004 != 0 {
                        r.warn(
                            format!("GitHub token file {} is world-readable", tp.display()),
                            Some(format!(
                                "chmod o-r \"{}\" on the host",
                                host_path("CONFIG_DIR")
                                    .map(|h| format!(
                                        "{h}/{}",
                                        tp.file_name().unwrap().to_string_lossy()
                                    ))
                                    .unwrap_or_else(|| tp.display().to_string())
                            )),
                        );
                    }
                    match cfg.read_token() {
                        Ok(Some(_)) => r.ok("GitHub token file is readable (value not shown)"),
                        Ok(None) => r.warn(
                            "GitHub token file is empty; unauthenticated rate limits apply",
                            None,
                        ),
                        Err(e) => r.fail(e.0.join("; "), None),
                    }
                }
                Err(e) => r.fail(format!("GitHub token file {}: {e}", tp.display()), None),
            }
        }
        // Web identity compatibility.
        match web_identity() {
            Err(e) => r.fail(e, None),
            Ok(None) => r.warn("CRAFT_WEB_UID/CRAFT_WEB_GID not provided; cannot check that the web server can read published files", None),
            Ok(Some(web)) => {
                if web.uid == 0 {
                    r.warn("the web server is configured to run as root", None);
                }
                if let Some(dm) = &data_meta {
                    let (ok, what) = new_file_access(&web, &me, dm, umask);
                    if ok {
                        r.ok(format!("web server {}:{} can read published files ({what})", web.uid, web.gid));
                    } else {
                        r.fail(
                            format!("web server {}:{} cannot read files the updater publishes ({what})", web.uid, web.gid),
                            Some(format!(
                                "use the same RUN_UID/RUN_GID for both services, or put the web server in group {} (WEB_SUPPLEMENTAL_GID) with a FILE_UMASK that keeps group read (e.g. 0002 or 0022), or setgid DATA_DIR to a shared group",
                                if dm.permissions().mode() & 0o2000 != 0 { dm.gid() } else { me.gid }
                            )),
                        );
                    }
                    if web.access(dm.uid(), dm.gid(), dm.permissions().mode()) & 0o5 != 0o5 {
                        r.fail(format!("web server {}:{} cannot enter DATA_DIR ({})", web.uid, web.gid, describe(dm)), Some(prepare_hint("DATA_DIR", &me, umask)));
                    }
                    let mut unreadable = Vec::new();
                    walk_unreadable(&paths.data, &web, 5, &mut unreadable);
                    if unreadable.is_empty() {
                        r.ok("existing published content is readable by the web server");
                    } else {
                        r.fail(format!("published paths unreadable by the web server: {}", unreadable.join("; ")), Some("fix group/mode on the host; the updater never changes ownership of existing files".into()));
                    }
                }
            }
        }
        // Network and validation listener.
        if network {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_global(Some(Duration::from_secs(cfg.updater.http_timeout_secs)))
                .build()
                .new_agent();
            let url = format!(
                "{}/rate_limit",
                cfg.updater.github_api_url.trim_end_matches('/')
            );
            let mut req = agent
                .get(&url)
                .header("User-Agent", crate::github::USER_AGENT);
            if let Ok(Some(t)) = cfg.read_token() {
                req = req.header("Authorization", format!("Bearer {t}"));
            }
            match req.call() {
                Ok(mut resp) if resp.status().as_u16() == 200 => {
                    let v: serde_json::Value = resp
                        .body_mut()
                        .read_to_string()
                        .ok()
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or_default();
                    let core = &v["resources"]["core"];
                    r.ok(format!(
                        "GitHub API reachable; {} of {} requests left this hour",
                        core["remaining"], core["limit"]
                    ));
                }
                Ok(resp) => r.warn(
                    format!("GitHub API answered HTTP {}", resp.status().as_u16()),
                    None,
                ),
                Err(e) => r.warn(
                    format!("GitHub API unreachable ({e}); installed apps keep being served"),
                    None,
                ),
            }
            if !cfg.updater.validation_url.is_empty() {
                let url = format!(
                    "{}/healthz",
                    cfg.updater.validation_url.trim_end_matches('/')
                );
                match agent.get(&url).call() {
                    Ok(resp) if resp.status().as_u16() == 200 => r.ok(format!("private validation listener reachable at {}", cfg.updater.validation_url)),
                    Ok(resp) => r.fail(format!("validation listener answered HTTP {}", resp.status().as_u16()), None),
                    Err(e) => r.warn(format!("validation listener {} unreachable ({e}); updates wait until the web service runs", cfg.updater.validation_url), None),
                }
            }
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_uses_owner_then_group_then_other_bits() {
        let web = Identity {
            uid: 101,
            gid: 101,
            groups: vec![10000],
        };
        assert_eq!(web.access(101, 5, 0o640), 0o6);
        assert_eq!(web.access(1000, 10000, 0o640), 0o4);
        assert_eq!(web.access(1000, 1000, 0o640), 0);
        assert_eq!(web.access(1000, 1000, 0o644), 0o4);
    }
}

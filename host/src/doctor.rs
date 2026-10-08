//! `doctor`: configuration, identity, permission, space, filesystem and administration-access
//! checks with actionable preparation instructions. Never changes ownership or permissions.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::access::AdminAuth;
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

/// The running process's numeric identity.
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

fn host_path(label: &str) -> Option<String> {
    std::env::var(format!("CRAFT_HOST_{label}"))
        .ok()
        .filter(|v| !v.trim().is_empty())
}

fn prepare_hint(label: &str, me: &Identity, umask: u32) -> String {
    let host = host_path(label).unwrap_or_else(|| format!("<{label} on the host>"));
    let mode = if umask & 0o020 == 0 { "2775" } else { "2750" };
    format!(
        "prepare on the host (once, as an administrator):\n  sudo install -d -o {} -g {} -m {mode} \"{host}\"",
        me.uid, me.gid
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

fn check_dir(r: &mut Report, label: &str, path: &Path, writable: bool, me: &Identity, umask: u32) {
    let meta = match fs::metadata(path) {
        Ok(m) if m.is_dir() => m,
        Ok(_) => {
            return r.fail(
                format!("{label} ({}) is not a directory", path.display()),
                None,
            );
        }
        Err(e) => {
            return r.fail(
                format!("{label} ({}) is not accessible: {e}", path.display()),
                Some(prepare_hint(label, me, umask)),
            );
        }
    };
    if fs::read_dir(path).is_err() {
        return r.fail(
            format!(
                "{label} ({}) is not readable by uid {} ({})",
                path.display(),
                me.uid,
                describe(&meta)
            ),
            Some(prepare_hint(label, me, umask)),
        );
    }
    if !writable {
        return r.ok(format!(
            "{label} ({}) is readable ({})",
            path.display(),
            describe(&meta)
        ));
    }
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

/// A secret file must be readable by the process and not by everyone.
fn check_secret_file(r: &mut Report, what: &str, path: &Path) {
    match fs::metadata(path) {
        Ok(m) => {
            if m.permissions().mode() & 0o004 != 0 {
                r.warn(
                    format!("{what} {} is world-readable", path.display()),
                    Some("chmod o-r it on the host".into()),
                );
            }
            match fs::read(path) {
                Ok(b) if b.iter().any(|c| !c.is_ascii_whitespace()) => {
                    r.ok(format!("{what} is readable (contents not shown)"))
                }
                Ok(_) => r.fail(format!("{what} {} is empty", path.display()), None),
                Err(e) => r.fail(format!("{what} {}: {e}", path.display()), None),
            }
        }
        Err(e) => r.fail(format!("{what} {}: {e}", path.display()), None),
    }
}

fn check_admin(r: &mut Report, cfg: &Config, network: bool) {
    let admin = &cfg.admin;
    match admin.auth {
        AdminAuth::Disabled => return r.ok("admin interface disabled ([admin] auth = \"disabled\")"),
        AdminAuth::None => r.warn(
            "admin interface has no authentication: anyone who reaches /admin can update, pin and roll back apps",
            Some("only use this behind a proxy that authenticates /admin, or choose basic, form, oidc or proxy".into()),
        ),
        AdminAuth::Basic | AdminAuth::Form => {
            check_secret_file(r, "admin users file", &admin.users_file);
            let users = crate::auth::Users::new(admin.users_file.clone());
            if users.is_empty() {
                r.fail(
                    format!("admin users file {} has no users", admin.users_file.display()),
                    Some("add lines `name:<hash>`; create hashes with `craft-host hash-password`".into()),
                );
            }
        }
        AdminAuth::Oidc => {
            if let Some(o) = &admin.oidc {
                if o.client_secret_file.exists() {
                    check_secret_file(r, "OIDC client secret", &o.client_secret_file);
                } else {
                    r.warn("no OIDC client secret file; logging in as a public client (PKCE only)", None);
                }
                if network {
                    let oidc = crate::oidc::Oidc::new(o.clone(), cfg.updater.http_timeout_secs);
                    match oidc.start() {
                        Ok(_) => r.ok(format!("OIDC provider {} reachable", o.issuer)),
                        Err(e) => r.fail(format!("OIDC provider {}: {e:#}", o.issuer), None),
                    }
                }
            }
        }
        AdminAuth::Proxy => r.ok(format!(
            "admin identities are taken from {} sent by {} trusted proxy network(s)",
            admin.proxy.user_header,
            cfg.server.trusted_proxies.len()
        )),
    }
    if admin.allowed_users.is_empty()
        && admin.allowed_groups.is_empty()
        && matches!(admin.auth, AdminAuth::Oidc | AdminAuth::Proxy)
    {
        r.warn("every authenticated user may manage updates; set [admin] allowed_users or allowed_groups", None);
    }
    match (&admin.host, admin.listen) {
        (Some(h), _) => r.ok(format!("admin interface only on host {h}, separate from the apps' origin")),
        (None, Some(a)) => r.ok(format!("admin interface only on its own listener {a}, separate from the apps' origin")),
        (None, None) => r.warn(
            "shared_origin = true: the admin interface shares a browser origin with the apps; code served by an app could use a signed-in admin session",
            Some("set [admin] listen to a dedicated port (or host to a dedicated host name) and remove shared_origin".into()),
        ),
    }
}

pub fn run(paths: Paths, network: bool) -> Report {
    let mut r = Report::default();
    let me = Identity::current();

    if me.uid == 0 {
        r.fail(
            "the server runs as root; set RUN_UID/RUN_GID to a non-root identity",
            None,
        );
    } else {
        r.ok(format!(
            "runs as uid {} gid {} groups {:?}",
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
    for var in ["RUN_UID", "RUN_GID"] {
        match parse_id(var) {
            Err(e) => r.fail(e, None),
            Ok(Some(0)) => r.warn(
                format!(
                    "{var}=0 configures root; non-root operation is the default and recommended"
                ),
                None,
            ),
            Ok(Some(id)) => {
                let actual = if var == "RUN_UID" { me.uid } else { me.gid };
                if id != actual {
                    r.warn(
                        format!("{var}={id} but the process runs with {actual}"),
                        Some("user-namespace remapping or Docker Desktop file sharing can change identities; see docs/permissions.md".into()),
                    );
                }
            }
            Ok(None) => {}
        }
    }

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

    check_dir(&mut r, "CONFIG_DIR", &paths.config_dir(), false, &me, umask);
    if probe_write(&paths.config_dir()).is_ok() {
        r.warn(
            "CONFIG_DIR is writable by the server; mount it read-only",
            None,
        );
    }
    for (label, p) in [
        ("DATA_DIR", &paths.data),
        ("STATE_DIR", &paths.state),
        ("CACHE_DIR", &paths.cache),
        ("WORK_DIR", &paths.work),
    ] {
        check_dir(&mut r, label, p, true, &me, umask);
    }
    if let Some(l) = &paths.logs {
        check_dir(&mut r, "LOG_DIR", l, true, &me, umask);
    }

    let Some(cfg) = cfg else { return r };
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
            Ok(free) => r.warn(
                format!("{label} has only {} MiB free; updates need room for downloads and two releases side by side", free >> 20),
                None,
            ),
            Err(_) => {}
        }
    }
    if let Ok(dm) = fs::metadata(&paths.data) {
        let mut ok = true;
        for app in &cfg.apps {
            if let Ok(m) = fs::metadata(cfg.release_root(app))
                && m.dev() != dm.dev()
            {
                ok = false;
                r.fail(
                    format!("{} ({}) is a separate filesystem from DATA_DIR; atomic promotion requires one filesystem", app.release_dir, app.id),
                    None,
                );
            }
        }
        if ok {
            r.ok("release directories share DATA_DIR's filesystem (atomic promotion possible)");
        }
    }
    if let Some(tp) = cfg.token_path() {
        check_secret_file(&mut r, "GitHub token file", &tp);
    }
    check_admin(&mut r, &cfg, network);

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
    }
    r
}

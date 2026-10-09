//! `doctor`: configuration, identity, permission, space, filesystem and administration-access
//! checks with actionable preparation instructions. Never changes ownership or permissions.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::access::{AppsAccess, Role, SignIn};
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

fn check_auth(r: &mut Report, cfg: &Config, network: bool) {
    let (auth, admin) = (&cfg.auth, &cfg.admin);
    let apps = match auth.apps {
        AppsAccess::Public => "the launcher and apps are public",
        AppsAccess::SignedIn => "the launcher and apps need a signed-in user or admin",
    };
    match (admin.enabled, &admin.host) {
        (false, _) => r.ok(format!("admin interface disabled; {apps}")),
        (true, Some(h)) => r.ok(format!("admin interface on host {h}; {apps}")),
        (true, None) => r.ok(format!(
            "admin interface at /admin/ on the apps' port and browser origin ([admin] host would separate it); {apps}"
        )),
    }
    match auth.sign_in {
        SignIn::None => {
            if admin.enabled {
                r.warn(
                    "[auth] methods is empty: anyone who reaches /admin can update, pin and roll back apps",
                    Some("only use this behind a proxy that authenticates /admin, or set [auth] methods".into()),
                );
            }
        }
        SignIn::Proxy => r.ok(format!(
            "identities are taken from {} sent by {} trusted proxy network(s)",
            auth.proxy.user_header,
            cfg.server.trusted_proxies.len()
        )),
        SignIn::Basic | SignIn::Interactive { .. } => check_accounts(r, cfg),
    }
    if let (true, Some(o)) = (auth.sign_in.oidc(), &auth.oidc) {
        if o.client_secret_file.exists() {
            check_secret_file(r, "OIDC client secret", &o.client_secret_file);
        } else {
            r.warn(
                "no OIDC client secret file; logging in as a public client (PKCE only)",
                None,
            );
        }
        if network {
            let oidc = crate::oidc::Oidc::new(o.clone(), cfg.updater.http_timeout_secs);
            match oidc.start(String::new(), None) {
                Ok(_) => r.ok(format!("OIDC provider {} reachable", o.issuer)),
                Err(e) => r.fail(format!("OIDC provider {}: {e:#}", o.issuer), None),
            }
        }
        if auth.provider_roles() {
            r.ok("roles of OIDC accounts follow the provider's groups at every sign-in");
        }
    }
}

/// The user database exists, is private to the server's user and group, and has an
/// administrator who can sign in.
fn check_accounts(r: &mut Report, cfg: &Config) {
    let path = cfg.paths.state.join(crate::users::DB_FILE);
    if !path.exists() {
        r.warn(
            format!("user database {} does not exist yet", path.display()),
            Some("create the first administrator: craft-host user add NAME --role admin".into()),
        );
        return;
    }
    if let Ok(m) = fs::metadata(&path)
        && m.permissions().mode() & 0o007 != 0
    {
        r.fail(
            format!(
                "user database {} is readable by other users",
                path.display()
            ),
            Some(format!("chmod 0660 {}", path.display())),
        );
    }
    let db = match crate::users::UserDb::open(&cfg.paths.state) {
        Ok(db) => db,
        Err(e) => return r.fail(format!("user database: {e:#}"), None),
    };
    let users = db.list().unwrap_or_default();
    let admins: Vec<_> = users.iter().filter(|u| u.role == Role::Admin).collect();
    let sign_in = cfg.auth.sign_in;
    // An admin can sign in with a password (local, basic) or with a linked or linkable OIDC
    // identity; with provider roles the provider can make anyone an admin.
    let usable = admins.iter().any(|u| {
        (u.has_password && (sign_in.local() || sign_in == SignIn::Basic))
            || (sign_in.oidc() && u.oidc.is_some())
    }) || (sign_in.oidc() && cfg.auth.provider_roles());
    r.ok(format!(
        "user database: {} account(s), {} administrator(s)",
        users.len(),
        admins.len()
    ));
    if cfg.admin.enabled && !usable {
        r.fail(
            "no administrator can sign in with the configured [auth] methods",
            Some("craft-host user add NAME --role admin (password on standard input), or craft-host user set-password NAME".into()),
        );
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
    check_auth(&mut r, &cfg, network);

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

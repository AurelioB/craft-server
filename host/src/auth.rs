//! Sign-in building blocks: users file with roles (Basic and form login), sessions, trusted-proxy
//! identities, cookies, CSRF protection and safe post-login redirects. OIDC lives in `oidc.rs`,
//! the HTTP handlers in `login.rs`.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::SystemTime;

use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use axum::http::HeaderMap;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use crate::access::{AuthSettings, CookieSecure, Role, ServerSettings};
use crate::fsutil::random_hex;
use crate::timeutil::now_epoch;

pub const SESSION_COOKIE: &str = "craft_session";
pub const CSRF_COOKIE: &str = "craft_csrf";
pub const CSRF_HEADER: &str = "x-craft-csrf";
/// Successful Basic credentials are remembered briefly so each request does not pay for Argon2.
const VERIFIED_CACHE_SECS: i64 = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub user: String,
    pub method: &'static str,
    pub role: Role,
}

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("cannot hash password: {e}"))
}

#[derive(Debug, Clone)]
struct UserEntry {
    name: String,
    hash: String,
    role: Role,
}

/// Users file contents as of a modification time.
type LoadedUsers = (Option<SystemTime>, Vec<UserEntry>);

/// Parse one `name:<PHC hash>[:role]` line; the role is `user` (default) or `admin`.
/// PHC strings never contain `:`.
fn parse_user_line(line: &str) -> Option<UserEntry> {
    let mut fields = line.splitn(3, ':');
    let name = fields.next()?.trim();
    let hash = fields.next()?.trim();
    let role = match fields.next().map(str::trim) {
        None | Some("") => Role::User,
        Some(r) => Role::parse(r)?,
    };
    (!name.is_empty() && !hash.is_empty()).then(|| UserEntry {
        name: name.to_string(),
        hash: hash.to_string(),
        role,
    })
}

/// `name:$argon2id$…[:role]` lines; `#` comments and blank lines are ignored, and so are lines
/// with an unknown role (`doctor` reports them).
pub struct Users {
    path: PathBuf,
    loaded: Mutex<Option<LoadedUsers>>,
    verified: Mutex<HashMap<[u8; 32], i64>>,
}

impl Users {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            loaded: Mutex::new(None),
            verified: Mutex::new(HashMap::new()),
        }
    }

    fn entries(&self) -> Vec<UserEntry> {
        let mtime = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        let mut loaded = self.loaded.lock();
        if loaded.as_ref().is_none_or(|(t, _)| *t != mtime) {
            let text = std::fs::read_to_string(&self.path).unwrap_or_default();
            let entries = text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .filter_map(parse_user_line)
                .collect();
            *loaded = Some((mtime, entries));
            self.verified.lock().clear();
        }
        loaded.as_ref().map(|(_, e)| e.clone()).unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.entries().is_empty()
    }

    pub fn has_admin(&self) -> bool {
        self.entries().iter().any(|e| e.role == Role::Admin)
    }

    /// Lines that are neither blank, comments nor valid entries (for `doctor`).
    pub fn invalid_lines(&self) -> Vec<usize> {
        let text = std::fs::read_to_string(&self.path).unwrap_or_default();
        text.lines()
            .enumerate()
            .filter(|(_, l)| {
                let l = l.trim();
                !l.is_empty() && !l.starts_with('#') && parse_user_line(l).is_none()
            })
            .map(|(i, _)| i + 1)
            .collect()
    }

    /// The account's role if the password is right. Constant work for unknown users: a dummy
    /// hash is verified so timing does not reveal names.
    pub fn verify(&self, user: &str, password: &str) -> Option<Role> {
        let key: [u8; 32] = Sha256::digest(format!("{user}\0{password}").as_bytes()).into();
        let now = now_epoch();
        // Reloading on change also clears remembered verifications.
        let entries = self.entries();
        let entry = entries.iter().find(|e| e.name == user);
        if self
            .verified
            .lock()
            .get(&key)
            .is_some_and(|t| now - t < VERIFIED_CACHE_SECS)
        {
            return entry.map(|e| e.role);
        }
        let hash = entry.map(|e| e.hash.as_str());
        static DUMMY: std::sync::LazyLock<String> =
            std::sync::LazyLock::new(|| hash_password("timing equaliser").unwrap_or_default());
        let ok = Argon2::default()
            .verify_password(password.as_bytes(), hash.unwrap_or(DUMMY.as_str()))
            .is_ok()
            && hash.is_some();
        if ok {
            self.verified.lock().insert(key, now);
        }
        entry.filter(|_| ok).map(|e| e.role)
    }
}

struct Session {
    principal: Principal,
    expires: i64,
}

#[derive(Default)]
pub struct Sessions {
    map: Mutex<HashMap<String, Session>>,
}

impl Sessions {
    pub fn create(&self, principal: Principal, ttl: u64) -> String {
        let token = random_hex(32);
        let now = now_epoch();
        let mut map = self.map.lock();
        map.retain(|_, s| s.expires > now);
        map.insert(
            token.clone(),
            Session {
                principal,
                expires: now + ttl as i64,
            },
        );
        token
    }

    pub fn get(&self, token: &str) -> Option<Principal> {
        let map = self.map.lock();
        map.get(token)
            .filter(|s| s.expires > now_epoch())
            .map(|s| s.principal.clone())
    }

    pub fn remove(&self, token: &str) {
        self.map.lock().remove(token);
    }
}

pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(axum::http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// The scheme the client used: from X-Forwarded-Proto when the peer is a trusted proxy.
pub fn client_is_https(headers: &HeaderMap, peer: IpAddr, server: &ServerSettings) -> bool {
    server.is_trusted(peer)
        && header(headers, "x-forwarded-proto").is_some_and(|p| {
            p.split(',')
                .next()
                .is_some_and(|p| p.trim().eq_ignore_ascii_case("https"))
        })
}

/// Host the client asked for: X-Forwarded-Host from a trusted proxy, else Host. Lowercase.
pub fn request_host(headers: &HeaderMap, peer: IpAddr, server: &ServerSettings) -> Option<String> {
    let forwarded = server
        .is_trusted(peer)
        .then(|| header(headers, "x-forwarded-host"))
        .flatten();
    forwarded.or_else(|| header(headers, "host")).map(|h| {
        h.split(',')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
    })
}

/// The CSRF cookie: readable by the admin page's script, sent only to /admin.
pub fn csrf_cookie(value: &str, secure: bool) -> String {
    let mut c = format!("{CSRF_COOKIE}={value}; Path=/admin; SameSite=Strict");
    if secure {
        c.push_str("; Secure");
    }
    c
}

/// The session cookie, for the whole site. `Lax` so that following a link to an app from another
/// site, or returning from the identity provider, still carries it; every state-changing
/// request is a POST with its own CSRF and Origin checks.
pub fn session_cookie(value: &str, max_age: u64, secure: bool) -> String {
    let mut c =
        format!("{SESSION_COOKIE}={value}; Path=/; SameSite=Lax; HttpOnly; Max-Age={max_age}");
    if secure {
        c.push_str("; Secure");
    }
    c
}

pub fn cookie_secure(auth: &AuthSettings, https: bool) -> bool {
    match auth.cookie_secure {
        CookieSecure::Always => true,
        CookieSecure::Never => false,
        CookieSecure::Auto => https,
    }
}

/// `Authorization: Basic …` credentials.
pub fn basic_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let v = header(headers, "authorization")?;
    let (scheme, rest) = v.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = String::from_utf8(STANDARD.decode(rest.trim()).ok()?).ok()?;
    let (u, p) = decoded.split_once(':')?;
    Some((u.to_string(), p.to_string()))
}

/// Identity asserted by a trusted reverse proxy. Requests from other peers never carry one.
pub fn proxy_identity(
    headers: &HeaderMap,
    peer: IpAddr,
    server: &ServerSettings,
    auth: &AuthSettings,
) -> Option<(String, Vec<String>)> {
    if !server.is_trusted(peer) {
        return None;
    }
    let user = header(headers, &auth.proxy.user_header)?.trim();
    if user.is_empty() {
        return None;
    }
    let groups = header(headers, &auth.proxy.groups_header)
        .map(|g| {
            g.split(['|', ','])
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    Some((user.to_string(), groups))
}

/// State-changing requests must echo the CSRF cookie in a header (double submit) and, when the
/// browser sends Origin, come from the same host.
pub fn csrf_ok(headers: &HeaderMap, host: Option<&str>) -> bool {
    let (Some(c), Some(h)) = (cookie(headers, CSRF_COOKIE), header(headers, CSRF_HEADER)) else {
        return false;
    };
    if c.len() < 32 || c != h {
        return false;
    }
    match (header(headers, "origin"), host) {
        (Some(origin), Some(host)) => origin
            .split("://")
            .nth(1)
            .is_some_and(|o| o.eq_ignore_ascii_case(host)),
        (Some(_), None) => false,
        (None, _) => true,
    }
}

/// `Origin`, when the browser sends one, names the host the request was addressed to.
pub fn same_origin(headers: &HeaderMap, host: Option<&str>) -> bool {
    match (header(headers, "origin"), host) {
        (Some(o), Some(h)) => o
            .split("://")
            .nth(1)
            .is_some_and(|o| o.eq_ignore_ascii_case(h)),
        (Some(_), None) => false,
        (None, _) => true,
    }
}

/// Percent-encode everything but RFC 3986 unreserved characters.
pub fn query_escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// `../` steps from a request path back to the site root, so redirects stay relative (they work
/// behind proxies that rewrite the host or port).
pub fn to_root(path: &str) -> String {
    "../".repeat(path.matches('/').count().saturating_sub(1))
}

/// Where to go after signing in: a site-relative path without a leading slash, taken from the
/// `next` parameter. Anything that could leave the site falls back to the launcher.
pub fn safe_next(next: Option<&str>) -> String {
    let n = next.unwrap_or("");
    let safe = !n.starts_with(['/', '\\'])
        && !n.contains(['\\', '\r', '\n'])
        && !n.contains("://")
        && !n.starts_with("auth/");
    if safe { n.to_string() } else { String::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn users_file_verifies_hashes_and_reloads_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin-users");
        std::fs::write(
            &path,
            format!(
                "# accounts\nana:{}:admin\ncy:{}\nbad:{}:root\n",
                hash_password("correct horse").unwrap(),
                hash_password("cy-pw").unwrap(),
                hash_password("x").unwrap()
            ),
        )
        .unwrap();
        let users = Users::new(path.clone());
        assert_eq!(users.verify("ana", "correct horse"), Some(Role::Admin));
        assert_eq!(
            users.verify("cy", "cy-pw"),
            Some(Role::User),
            "role defaults to user"
        );
        assert_eq!(users.verify("ana", "wrong"), None);
        assert_eq!(users.verify("bob", "correct horse"), None);
        assert_eq!(users.verify("bad", "x"), None, "unknown role: line ignored");
        assert_eq!(users.invalid_lines(), vec![4]);
        assert!(users.has_admin());
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            &path,
            format!("bob:{}:user\n", hash_password("pw").unwrap()),
        )
        .unwrap();
        let t = SystemTime::now();
        filetime_touch(&path, t);
        assert_eq!(
            users.verify("ana", "correct horse"),
            None,
            "removed user no longer accepted"
        );
        assert_eq!(users.verify("bob", "pw"), Some(Role::User));
        assert!(!users.has_admin());
    }

    #[test]
    fn post_login_targets_stay_on_the_site() {
        assert_eq!(
            safe_next(Some("photocraft/0.5.0/?x=1")),
            "photocraft/0.5.0/?x=1"
        );
        assert_eq!(safe_next(Some("admin/")), "admin/");
        for bad in [
            "//evil.example/",
            "/x",
            "\\evil",
            "https://evil.example/",
            "auth/login",
            "a\nb",
        ] {
            assert_eq!(safe_next(Some(bad)), "", "{bad:?}");
        }
        assert_eq!(to_root("/"), "");
        assert_eq!(to_root("/admin/"), "../");
        assert_eq!(to_root("/photocraft/0.5.0/index.html"), "../../");
    }

    fn filetime_touch(path: &std::path::Path, t: SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    #[test]
    fn proxy_headers_are_ignored_from_untrusted_peers() {
        let server = crate::access::build_server(
            toml::from_str("trusted_proxies = [\"10.0.0.0/8\"]").unwrap(),
            &mut vec![],
        );
        let admin = crate::access::build_admin(Default::default(), &mut vec![]);
        let auth = crate::access::build_auth(
            Default::default(),
            std::path::Path::new("/c"),
            &server,
            &admin,
            &mut vec![],
        );
        let h = headers(&[("remote-user", "ana"), ("remote-groups", "admins|ops")]);
        assert_eq!(
            proxy_identity(&h, "10.1.2.3".parse().unwrap(), &server, &auth),
            Some(("ana".into(), vec!["admins".into(), "ops".into()]))
        );
        assert_eq!(
            proxy_identity(&h, "192.168.1.5".parse().unwrap(), &server, &auth),
            None
        );
        let fwd = headers(&[
            ("host", "internal:8080"),
            ("x-forwarded-host", "apps.example.net"),
            ("x-forwarded-proto", "https"),
        ]);
        assert_eq!(
            request_host(&fwd, "10.0.0.1".parse().unwrap(), &server).as_deref(),
            Some("apps.example.net")
        );
        assert_eq!(
            request_host(&fwd, "192.168.0.1".parse().unwrap(), &server).as_deref(),
            Some("internal:8080")
        );
        assert!(client_is_https(&fwd, "10.0.0.1".parse().unwrap(), &server));
        assert!(!client_is_https(
            &fwd,
            "192.168.0.1".parse().unwrap(),
            &server
        ));
    }

    #[test]
    fn csrf_requires_matching_cookie_header_and_origin() {
        let token = "a".repeat(64);
        let ok = headers(&[
            ("cookie", &format!("x=1; {CSRF_COOKIE}={token}")),
            (CSRF_HEADER, &token),
            ("origin", "https://apps.example.net"),
        ]);
        assert!(csrf_ok(&ok, Some("apps.example.net")));
        assert!(!csrf_ok(&ok, Some("evil.example.net")));
        let missing = headers(&[("cookie", &format!("{CSRF_COOKIE}={token}"))]);
        assert!(!csrf_ok(&missing, Some("apps.example.net")));
        let mismatch = headers(&[
            ("cookie", &format!("{CSRF_COOKIE}={token}")),
            (CSRF_HEADER, &"b".repeat(64)),
        ]);
        assert!(!csrf_ok(&mismatch, None));
    }

    #[test]
    fn basic_credentials_are_decoded() {
        let h = headers(&[(
            "authorization",
            &format!("Basic {}", STANDARD.encode("ana:p:w")),
        )]);
        assert_eq!(basic_credentials(&h), Some(("ana".into(), "p:w".into())));
        assert_eq!(
            basic_credentials(&headers(&[("authorization", "Bearer x")])),
            None
        );
    }
}

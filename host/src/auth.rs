//! Administration authentication: users file (Basic and form login), sessions, trusted-proxy
//! identities and CSRF protection. OIDC lives in `oidc.rs`.

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

use crate::access::{AdminSettings, CookieSecure, ServerSettings};
use crate::fsutil::random_hex;
use crate::timeutil::now_epoch;

pub const SESSION_COOKIE: &str = "craft_admin";
pub const CSRF_COOKIE: &str = "craft_csrf";
pub const CSRF_HEADER: &str = "x-craft-csrf";
/// Successful Basic credentials are remembered briefly so each request does not pay for Argon2.
const VERIFIED_CACHE_SECS: i64 = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub user: String,
    pub method: &'static str,
}

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("cannot hash password: {e}"))
}

/// Users file contents as of a modification time: (user, PHC hash) pairs.
type LoadedUsers = (Option<SystemTime>, Vec<(String, String)>);

/// `name:$argon2id$…` lines; `#` comments and blank lines are ignored.
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

    fn entries(&self) -> Vec<(String, String)> {
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
                .filter_map(|l| l.split_once(':'))
                .map(|(u, h)| (u.trim().to_string(), h.trim().to_string()))
                .collect();
            *loaded = Some((mtime, entries));
            self.verified.lock().clear();
        }
        loaded.as_ref().map(|(_, e)| e.clone()).unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.entries().is_empty()
    }

    /// Constant work for unknown users: a dummy hash is verified so timing does not reveal names.
    pub fn verify(&self, user: &str, password: &str) -> bool {
        let key: [u8; 32] = Sha256::digest(format!("{user}\0{password}").as_bytes()).into();
        let now = now_epoch();
        // Reloading on change also clears remembered verifications.
        let entries = self.entries();
        if self
            .verified
            .lock()
            .get(&key)
            .is_some_and(|t| now - t < VERIFIED_CACHE_SECS)
        {
            return true;
        }
        let hash = entries
            .iter()
            .find(|(u, _)| u == user)
            .map(|(_, h)| h.as_str());
        static DUMMY: std::sync::LazyLock<String> =
            std::sync::LazyLock::new(|| hash_password("timing equaliser").unwrap_or_default());
        let ok = Argon2::default()
            .verify_password(password.as_bytes(), hash.unwrap_or(DUMMY.as_str()))
            .is_ok()
            && hash.is_some();
        if ok {
            self.verified.lock().insert(key, now);
        }
        ok
    }
}

struct Session {
    user: String,
    method: &'static str,
    expires: i64,
}

#[derive(Default)]
pub struct Sessions {
    map: Mutex<HashMap<String, Session>>,
}

impl Sessions {
    pub fn create(&self, user: &str, method: &'static str, ttl: u64) -> String {
        let token = random_hex(32);
        let now = now_epoch();
        let mut map = self.map.lock();
        map.retain(|_, s| s.expires > now);
        map.insert(
            token.clone(),
            Session {
                user: user.into(),
                method,
                expires: now + ttl as i64,
            },
        );
        token
    }

    pub fn get(&self, token: &str) -> Option<Principal> {
        let map = self.map.lock();
        map.get(token)
            .filter(|s| s.expires > now_epoch())
            .map(|s| Principal {
                user: s.user.clone(),
                method: s.method,
            })
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

pub fn set_cookie(
    name: &str,
    value: &str,
    max_age: Option<u64>,
    http_only: bool,
    secure: bool,
) -> String {
    let mut c = format!("{name}={value}; Path=/admin; SameSite=Strict");
    if let Some(a) = max_age {
        c.push_str(&format!("; Max-Age={a}"));
    }
    if http_only {
        c.push_str("; HttpOnly");
    }
    if secure {
        c.push_str("; Secure");
    }
    c
}

pub fn cookie_secure(admin: &AdminSettings, https: bool) -> bool {
    match admin.cookie_secure {
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
    admin: &AdminSettings,
) -> Option<(String, Vec<String>)> {
    if !server.is_trusted(peer) {
        return None;
    }
    let user = header(headers, &admin.proxy.user_header)?.trim();
    if user.is_empty() {
        return None;
    }
    let groups = header(headers, &admin.proxy.groups_header)
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
                "# admins\nana:{}\n",
                hash_password("correct horse").unwrap()
            ),
        )
        .unwrap();
        let users = Users::new(path.clone());
        assert!(users.verify("ana", "correct horse"));
        assert!(!users.verify("ana", "wrong"));
        assert!(!users.verify("bob", "correct horse"));
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, format!("bob:{}\n", hash_password("pw").unwrap())).unwrap();
        let t = SystemTime::now();
        filetime_touch(&path, t);
        assert!(
            !users.verify("ana", "correct horse"),
            "removed user no longer accepted"
        );
        assert!(users.verify("bob", "pw"));
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
        let admin = crate::access::build_admin(
            Default::default(),
            std::path::Path::new("/c"),
            &server,
            &mut vec![],
        );
        let h = headers(&[("remote-user", "ana"), ("remote-groups", "admins|ops")]);
        assert_eq!(
            proxy_identity(&h, "10.1.2.3".parse().unwrap(), &server, &admin),
            Some(("ana".into(), vec!["admins".into(), "ops".into()]))
        );
        assert_eq!(
            proxy_identity(&h, "192.168.1.5".parse().unwrap(), &server, &admin),
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

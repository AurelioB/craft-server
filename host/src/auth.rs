//! Sign-in building blocks: sessions, trusted-proxy identities, cookies, CSRF protection and
//! safe post-login redirects. Accounts live in `users.rs`, OIDC in `oidc.rs`, the HTTP handlers
//! in `login.rs`.

use std::collections::HashMap;
use std::net::IpAddr;

use axum::http::HeaderMap;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use parking_lot::Mutex;

use crate::access::{AuthSettings, CookieSecure, Role, ServerSettings};
use crate::fsutil::random_hex;
use crate::timeutil::now_epoch;

pub const SESSION_COOKIE: &str = "craft_session";
pub const CSRF_COOKIE: &str = "craft_csrf";
pub const CSRF_HEADER: &str = "x-craft-csrf";
/// Failed sign-ins per client address within this window before further attempts are refused.
const FAILURE_WINDOW_SECS: i64 = 15 * 60;
const MAX_FAILURES: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// Database account, for local, OIDC and Basic sign-in.
    pub user_id: Option<i64>,
    pub user: String,
    pub method: &'static str,
    pub role: Role,
}

/// A signed-in browser: the account and the account's `session_version` at sign-in. The current
/// name and role are read from the database on every request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionRef {
    pub user_id: i64,
    pub version: i64,
    pub method: &'static str,
}

struct Session {
    at: SessionRef,
    expires: i64,
}

#[derive(Default)]
pub struct Sessions {
    map: Mutex<HashMap<String, Session>>,
}

impl Sessions {
    pub fn create(&self, at: SessionRef, ttl: u64) -> String {
        let token = random_hex(32);
        let now = now_epoch();
        let mut map = self.map.lock();
        map.retain(|_, s| s.expires > now);
        map.insert(
            token.clone(),
            Session {
                at,
                expires: now + ttl as i64,
            },
        );
        token
    }

    pub fn get(&self, token: &str) -> Option<SessionRef> {
        let map = self.map.lock();
        map.get(token)
            .filter(|s| s.expires > now_epoch())
            .map(|s| s.at)
    }

    pub fn remove(&self, token: &str) {
        self.map.lock().remove(token);
    }

    /// End every session of an account (deleted, or its sign-in changed).
    pub fn remove_user(&self, user_id: i64) {
        self.map.lock().retain(|_, s| s.at.user_id != user_id);
    }
}

/// Recent failed sign-ins per client address; enough failures refuse further attempts for a
/// while (on top of Argon2's cost and the delay after each failure).
#[derive(Default)]
pub struct Throttle {
    failures: Mutex<HashMap<IpAddr, Vec<i64>>>,
}

impl Throttle {
    pub fn blocked(&self, ip: IpAddr) -> bool {
        let now = now_epoch();
        let mut f = self.failures.lock();
        let list = f.entry(ip).or_default();
        list.retain(|t| now - t < FAILURE_WINDOW_SECS);
        list.len() >= MAX_FAILURES
    }

    pub fn fail(&self, ip: IpAddr) {
        let now = now_epoch();
        let mut f = self.failures.lock();
        f.retain(|_, l| l.last().is_some_and(|t| now - t < FAILURE_WINDOW_SECS));
        f.entry(ip).or_default().push(now);
    }

    pub fn succeed(&self, ip: IpAddr) {
        self.failures.lock().remove(&ip);
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
    fn repeated_failures_from_one_address_are_refused_for_a_while() {
        let t = Throttle::default();
        let (a, b): (IpAddr, IpAddr) = ("10.0.0.1".parse().unwrap(), "10.0.0.2".parse().unwrap());
        for _ in 0..MAX_FAILURES {
            assert!(!t.blocked(a));
            t.fail(a);
        }
        assert!(t.blocked(a));
        assert!(!t.blocked(b), "per address");
        t.succeed(a);
        assert!(!t.blocked(a));
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

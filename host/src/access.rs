//! Listener, sign-in and administration settings: `[server]`, `[auth]` (with `[auth.oidc]` and
//! `[auth.proxy]`) and `[admin]`.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// An IPv4 or IPv6 network in CIDR notation; a bare address is a single-host network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpNet {
    addr: IpAddr,
    prefix: u8,
}

impl IpNet {
    pub fn parse(text: &str) -> Option<Self> {
        let (a, p) = match text.trim().split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (text.trim(), None),
        };
        let addr: IpAddr = a.parse().ok()?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match p {
            Some(p) => p.parse::<u8>().ok().filter(|p| *p <= max)?,
            None => max,
        };
        Some(Self { addr, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
            v4 => v4,
        };
        match (self.addr, ip) {
            (IpAddr::V4(n), IpAddr::V4(i)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - self.prefix)
                };
                u32::from(n) & mask == u32::from(i) & mask
            }
            (IpAddr::V6(n), IpAddr::V6(i)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u128::MAX << (128 - self.prefix)
                };
                u128::from(n) & mask == u128::from(i) & mask
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServerSettings {
    pub listen: SocketAddr,
    /// Peers whose X-Forwarded-Proto/-For and proxy-auth headers are believed.
    pub trusted_proxies: Vec<IpNet>,
}

impl ServerSettings {
    pub fn is_trusted(&self, peer: IpAddr) -> bool {
        self.trusted_proxies.iter().any(|n| n.contains(peer))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    /// Nobody signs in; every visitor counts as an administrator (only behind a protecting proxy).
    None,
    /// HTTP Basic authentication against the users file.
    Basic,
    /// Login form with a session cookie, against the users file.
    Form,
    /// OpenID Connect authorization-code flow with PKCE.
    Oidc,
    /// Identity from a header set by a trusted reverse proxy (forward auth).
    Proxy,
}

impl AuthMethod {
    pub fn name(self) -> &'static str {
        match self {
            AuthMethod::None => "none",
            AuthMethod::Basic => "basic",
            AuthMethod::Form => "form",
            AuthMethod::Oidc => "oidc",
            AuthMethod::Proxy => "proxy",
        }
    }

    /// Methods with a session the user can end.
    pub fn has_sessions(self) -> bool {
        matches!(self, AuthMethod::Form | AuthMethod::Oidc)
    }
}

/// What a signed-in account may do. Administrators may also use the apps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    User,
    Admin,
}

impl Role {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "user" => Some(Role::User),
            "admin" => Some(Role::Admin),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Admin => "admin",
        }
    }
}

/// Who may open the launcher and the apps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppsAccess {
    /// Anyone who can reach the server (default).
    Public,
    /// Signed-in accounts with the user or admin role.
    SignedIn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CookieSecure {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone)]
pub struct OidcSettings {
    pub issuer: String,
    pub client_id: String,
    pub client_secret_file: PathBuf,
    pub redirect_url: String,
    pub scopes: Vec<String>,
    pub username_claim: String,
    pub groups_claim: String,
}

#[derive(Debug, Clone)]
pub struct ProxyAuthSettings {
    pub user_header: String,
    pub groups_header: String,
}

/// `[auth]`: how people sign in and which role they get.
#[derive(Debug, Clone)]
pub struct AuthSettings {
    pub method: AuthMethod,
    pub apps: AppsAccess,
    pub users_file: PathBuf,
    pub session_ttl_secs: u64,
    pub cookie_secure: CookieSecure,
    /// For oidc and proxy: accounts and groups with the admin role.
    pub admin_users: Vec<String>,
    pub admin_groups: Vec<String>,
    /// For oidc and proxy: groups with the user role; empty = every signed-in account.
    pub user_groups: Vec<String>,
    pub oidc: Option<OidcSettings>,
    pub proxy: ProxyAuthSettings,
}

impl AuthSettings {
    /// Role of an identity from OIDC or a trusted proxy; None = no access at all.
    pub fn role_for(&self, user: &str, groups: &[String]) -> Option<Role> {
        let in_any = |list: &[String]| groups.iter().any(|g| list.contains(g));
        if self.admin_users.iter().any(|u| u == user) || in_any(&self.admin_groups) {
            Some(Role::Admin)
        } else if self.user_groups.is_empty() || in_any(&self.user_groups) {
            Some(Role::User)
        } else {
            None
        }
    }
}

/// `[admin]`: the administration interface under /admin on the apps' port.
#[derive(Debug, Clone)]
pub struct AdminSettings {
    pub enabled: bool,
    /// If set, /admin answers only for this host name, and apps are not served on it.
    pub host: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawServer {
    listen: Option<String>,
    trusted_proxies: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawAuth {
    method: Option<String>,
    apps: Option<String>,
    users_file: Option<String>,
    session_ttl: Option<crate::config::Quantity>,
    cookie_secure: Option<String>,
    admin_users: Option<Vec<String>>,
    admin_groups: Option<Vec<String>>,
    user_groups: Option<Vec<String>>,
    oidc: Option<RawOidc>,
    proxy: Option<RawProxy>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawAdmin {
    enabled: Option<bool>,
    host: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawOidc {
    issuer: Option<String>,
    client_id: Option<String>,
    client_secret_file: Option<String>,
    redirect_url: Option<String>,
    scopes: Option<Vec<String>>,
    username_claim: Option<String>,
    groups_claim: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawProxy {
    user_header: Option<String>,
    groups_header: Option<String>,
}

fn config_path(config_dir: &Path, name: &str) -> PathBuf {
    let p = PathBuf::from(name);
    if p.is_absolute() {
        p
    } else {
        config_dir.join(p)
    }
}

fn valid_header_name(h: &str) -> bool {
    !h.is_empty()
        && h.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

fn valid_host(h: &str) -> bool {
    let (host, port) = match h.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (h, None),
    };
    port.is_none_or(|p| p.parse::<u16>().is_ok_and(|n| n > 0))
        && !host.is_empty()
        && host
            .split('.')
            .all(|l| !l.is_empty() && l.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'))
}

pub(crate) fn build_server(raw: RawServer, problems: &mut Vec<String>) -> ServerSettings {
    let listen_text = raw.listen.unwrap_or_else(|| "0.0.0.0:8080".into());
    let listen = listen_text.parse().unwrap_or_else(|_| {
        problems.push(format!(
            "[server] listen: {listen_text:?} is not an address:port such as \"0.0.0.0:8080\""
        ));
        SocketAddr::from(([0, 0, 0, 0], 8080))
    });
    let mut trusted_proxies = Vec::new();
    for t in raw.trusted_proxies.unwrap_or_default() {
        match IpNet::parse(&t) {
            Some(n) => trusted_proxies.push(n),
            None => problems.push(format!(
                "[server] trusted_proxies: {t:?} is not an address or CIDR network"
            )),
        }
    }
    ServerSettings {
        listen,
        trusted_proxies,
    }
}

pub(crate) fn build_admin(raw: RawAdmin, problems: &mut Vec<String>) -> AdminSettings {
    let host = raw
        .host
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| !h.is_empty());
    if let Some(h) = &host
        && !valid_host(h)
    {
        problems.push(format!("[admin] host: {h:?} must be a host name such as \"admin.example.net\" (optionally with :port)"));
    }
    AdminSettings {
        enabled: raw.enabled.unwrap_or(false),
        host,
    }
}

pub(crate) fn build_auth(
    raw: RawAuth,
    config_dir: &Path,
    server: &ServerSettings,
    admin: &AdminSettings,
    problems: &mut Vec<String>,
) -> AuthSettings {
    let method = match raw.method.as_deref().unwrap_or("none") {
        "none" => AuthMethod::None,
        "basic" => AuthMethod::Basic,
        "form" => AuthMethod::Form,
        "oidc" => AuthMethod::Oidc,
        "proxy" => AuthMethod::Proxy,
        other => {
            problems.push(format!(
                "[auth] method: {other:?} is not one of none, basic, form, oidc, proxy"
            ));
            AuthMethod::None
        }
    };
    let apps = match raw.apps.as_deref().unwrap_or("public") {
        "public" => AppsAccess::Public,
        "signed-in" => AppsAccess::SignedIn,
        other => {
            problems.push(format!(
                "[auth] apps: {other:?} is not one of public, signed-in"
            ));
            AppsAccess::Public
        }
    };
    if apps == AppsAccess::SignedIn && method == AuthMethod::None {
        problems.push(
            "[auth] apps = \"signed-in\" needs a sign-in method: set [auth] method to basic, form, oidc or proxy".into(),
        );
    }
    let cookie_secure = match raw.cookie_secure.as_deref().unwrap_or("auto") {
        "auto" => CookieSecure::Auto,
        "always" => CookieSecure::Always,
        "never" => CookieSecure::Never,
        other => {
            problems.push(format!(
                "[auth] cookie_secure: {other:?} is not one of auto, always, never"
            ));
            CookieSecure::Auto
        }
    };
    let session_ttl_secs =
        crate::config::parse_duration(raw.session_ttl, "[auth] session_ttl", 12 * 3600, problems);
    if session_ttl_secs < 60 {
        problems.push("[auth] session_ttl: must be at least 1 minute".into());
    }
    let p = raw.proxy.unwrap_or_default();
    let proxy = ProxyAuthSettings {
        user_header: p.user_header.unwrap_or_else(|| "Remote-User".into()),
        groups_header: p.groups_header.unwrap_or_else(|| "Remote-Groups".into()),
    };
    for (k, h) in [
        ("user_header", &proxy.user_header),
        ("groups_header", &proxy.groups_header),
    ] {
        if !valid_header_name(h) {
            problems.push(format!("[auth.proxy] {k}: {h:?} is not a header name"));
        }
    }
    if method == AuthMethod::Proxy && server.trusted_proxies.is_empty() {
        problems.push("[auth] method = \"proxy\" needs [server] trusted_proxies, otherwise any client could claim an identity".into());
    }
    let admin_users = raw.admin_users.unwrap_or_default();
    let admin_groups = raw.admin_groups.unwrap_or_default();
    if admin.enabled
        && matches!(method, AuthMethod::Oidc | AuthMethod::Proxy)
        && admin_users.is_empty()
        && admin_groups.is_empty()
    {
        problems.push(format!(
            "[auth] admin_groups or admin_users: required with method = \"{}\" while [admin] is enabled, so that only chosen accounts manage updates",
            method.name()
        ));
    }
    let oidc = raw.oidc.map(|o| OidcSettings {
        issuer: o
            .issuer
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_string(),
        client_id: o.client_id.unwrap_or_default(),
        client_secret_file: config_path(
            config_dir,
            &o.client_secret_file
                .unwrap_or_else(|| "oidc-client-secret".into()),
        ),
        redirect_url: o.redirect_url.unwrap_or_default(),
        scopes: o
            .scopes
            .unwrap_or_else(|| vec!["openid".into(), "profile".into(), "email".into()]),
        username_claim: o
            .username_claim
            .unwrap_or_else(|| "preferred_username".into()),
        groups_claim: o.groups_claim.unwrap_or_else(|| "groups".into()),
    });
    if method == AuthMethod::Oidc && admin.host.is_some() {
        problems.push("[admin] host: cannot be combined with [auth] method = \"oidc\": sign-in completes on the single redirect_url host, so the admin host would never get a session".into());
    }
    if method == AuthMethod::Oidc {
        match &oidc {
            None => problems.push("[auth] method = \"oidc\" needs an [auth.oidc] section".into()),
            Some(o) => {
                if !o.issuer.starts_with("https://") && !o.issuer.starts_with("http://") {
                    problems.push("[auth.oidc] issuer: must be the provider's issuer URL".into());
                }
                if o.client_id.is_empty() {
                    problems.push("[auth.oidc] client_id: required".into());
                }
                if !o.redirect_url.ends_with("/auth/oidc/callback")
                    || !(o.redirect_url.starts_with("https://")
                        || o.redirect_url.starts_with("http://"))
                {
                    problems.push("[auth.oidc] redirect_url: must be the public URL ending in /auth/oidc/callback".into());
                }
                if !o.scopes.iter().any(|s| s == "openid") {
                    problems.push("[auth.oidc] scopes: must include \"openid\"".into());
                }
            }
        }
    }
    AuthSettings {
        method,
        apps,
        users_file: config_path(
            config_dir,
            &raw.users_file.unwrap_or_else(|| "users".into()),
        ),
        session_ttl_secs,
        cookie_secure,
        admin_users,
        admin_groups,
        user_groups: raw.user_groups.unwrap_or_default(),
        oidc,
        proxy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_matching_covers_v4_v6_and_mapped_addresses() {
        let n = IpNet::parse("172.16.0.0/12").unwrap();
        assert!(n.contains("172.20.1.2".parse().unwrap()));
        assert!(!n.contains("172.32.0.1".parse().unwrap()));
        assert!(n.contains("::ffff:172.17.0.1".parse().unwrap()));
        assert!(
            IpNet::parse("fd00::/8")
                .unwrap()
                .contains("fd12::1".parse().unwrap())
        );
        assert!(
            IpNet::parse("10.0.0.5")
                .unwrap()
                .contains("10.0.0.5".parse().unwrap())
        );
        assert!(
            !IpNet::parse("10.0.0.5")
                .unwrap()
                .contains("10.0.0.6".parse().unwrap())
        );
        assert!(IpNet::parse("10.0.0.0/33").is_none());
        assert!(IpNet::parse("example").is_none());
    }

    #[test]
    fn roles_come_from_admin_lists_then_user_groups() {
        let server = build_server(RawServer::default(), &mut vec![]);
        let admin = build_admin(RawAdmin::default(), &mut vec![]);
        let mut a = build_auth(
            RawAuth::default(),
            Path::new("/config"),
            &server,
            &admin,
            &mut vec![],
        );
        // No lists: every signed-in account is a user, nobody an admin.
        assert_eq!(a.role_for("anyone", &[]), Some(Role::User));
        a.admin_groups = vec!["craft-admins".into()];
        a.admin_users = vec!["ana".into()];
        assert_eq!(
            a.role_for("bo", &["craft-admins".into()]),
            Some(Role::Admin)
        );
        assert_eq!(a.role_for("ana", &[]), Some(Role::Admin));
        a.user_groups = vec!["craft-users".into()];
        assert_eq!(a.role_for("bo", &["craft-users".into()]), Some(Role::User));
        assert_eq!(a.role_for("bo", &["staff".into()]), None);
        // Admin membership wins even without a user group.
        assert_eq!(
            a.role_for("cy", &["craft-admins".into()]),
            Some(Role::Admin)
        );
    }
}

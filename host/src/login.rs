//! Sign-in for the whole site, per `[auth]`: who the requester is ([`authenticate`]), the `/auth/`
//! endpoints (sign-in page, local accounts, OIDC, linking, sign-out, `me`) and the gate in front
//! of the launcher and the apps when `[auth] apps = "signed-in"`.
//!
//! Redirects are relative to the request path so they work behind proxies that change the host
//! or port.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::extract::{ConnectInfo, Form, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode, header};
use axum::middleware::Next;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use crate::access::{AppsAccess, Role, SignIn};
use crate::auth::{
    Principal, SESSION_COOKIE, SessionRef, basic_credentials, client_is_https, cookie,
    cookie_secure, proxy_identity, query_escape, request_host, safe_next, same_origin,
    session_cookie, to_root,
};
use crate::serve::{redirect, set_common_headers, simple};
use crate::server::Shared;
use crate::users::{OidcClaims, OidcOutcome, User, UserError};

const LOGIN_HTML: &str = include_str!("../assets/login.html");
const LOGIN_CSS: &str = include_str!("../assets/admin.css");
/// Ties an OIDC callback to the browser that started the login (against login CSRF).
const OIDC_STATE_COOKIE: &str = "craft_oidc_state";

pub struct Ctx {
    pub peer: std::net::IpAddr,
    pub host: Option<String>,
    pub https: bool,
}

pub fn ctx(s: &Shared, headers: &HeaderMap, peer: SocketAddr) -> Ctx {
    Ctx {
        peer: peer.ip(),
        host: request_host(headers, peer.ip(), &s.cfg.server),
        https: client_is_https(headers, peer.ip(), &s.cfg.server),
    }
}

pub enum Denied {
    /// Not signed in; the sign-in page can fix that.
    Login,
    /// Basic: ask the browser for credentials.
    Challenge,
    Forbidden(&'static str),
}

fn principal(u: User, method: &'static str) -> Principal {
    Principal {
        user_id: Some(u.id),
        user: u.username,
        method,
        role: u.role,
    }
}

fn db_failed(e: UserError) -> Denied {
    log::error!("auth: user database: {e}");
    Denied::Forbidden("the user database is unavailable")
}

/// The signed-in account, if any. With no sign-in method everyone is an anonymous admin.
/// Sessions are checked against the database on every request, so deleted accounts, role
/// changes and new passwords (in this process or from the CLI) apply at once.
pub fn authenticate(s: &Shared, headers: &HeaderMap, c: &Ctx) -> Result<Principal, Denied> {
    let auth = &s.cfg.auth;
    match auth.sign_in {
        SignIn::None => Ok(Principal {
            user_id: None,
            user: "anonymous".into(),
            method: "none",
            role: Role::Admin,
        }),
        SignIn::Basic => match basic_credentials(headers) {
            Some((u, p)) => match s.users.verify_password(&u, &p).map_err(db_failed)? {
                Some(user) => Ok(principal(user, "basic")),
                None => Err(Denied::Challenge),
            },
            None => Err(Denied::Challenge),
        },
        SignIn::Interactive { .. } => {
            let Some(at) = cookie(headers, SESSION_COOKIE).and_then(|t| s.sessions.get(&t)) else {
                return Err(Denied::Login);
            };
            match s.users.get(at.user_id).map_err(db_failed)? {
                Some(user) if user.session_version == at.version => Ok(principal(user, at.method)),
                _ => Err(Denied::Login),
            }
        }
        SignIn::Proxy => match proxy_identity(headers, c.peer, &s.cfg.server, auth) {
            Some((u, g)) => match auth.role_for(&u, &g) {
                Some(role) => Ok(Principal {
                    user_id: None,
                    user: u,
                    method: "proxy",
                    role,
                }),
                None => Err(Denied::Forbidden("this account has no access")),
            },
            None => Err(Denied::Forbidden("no identity from a trusted proxy")),
        },
    }
}

/// Response for a browser page that needs a signed-in account. `path` and `query` are the
/// request's, so the user comes back to the same page after signing in.
pub fn denied_page(s: &Shared, d: Denied, path: &str, query: Option<&str>) -> Response<Body> {
    match d {
        Denied::Login => {
            let next = path.trim_start_matches('/').to_string()
                + &query.map(|q| format!("?{q}")).unwrap_or_default();
            // OIDC as the only method: straight to the provider.
            let page = match s.cfg.auth.sign_in {
                SignIn::Interactive {
                    local: false,
                    oidc: true,
                } => "auth/oidc/login",
                _ => "auth/login",
            };
            redirect(
                StatusCode::SEE_OTHER,
                &format!("{}{page}?next={}", to_root(path), query_escape(&next)),
            )
        }
        Denied::Challenge => {
            let mut r = simple(StatusCode::UNAUTHORIZED, "authentication required\n");
            r.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"Craft Apps\", charset=\"UTF-8\""),
            );
            r
        }
        Denied::Forbidden(m) => {
            let mut r = simple(StatusCode::FORBIDDEN, "forbidden\n");
            *r.body_mut() = Body::from(format!("forbidden: {m}\n"));
            r
        }
    }
}

/// Requests a browser makes to show a page (as opposed to scripts, images, workers…).
fn is_navigation(headers: &HeaderMap) -> bool {
    let h = |n: &str| headers.get(n).and_then(|v| v.to_str().ok());
    match h("sec-fetch-mode") {
        Some(mode) => mode == "navigate",
        None => h("accept").is_some_and(|a| a.contains("text/html")),
    }
}

/// Paths that are never behind the apps gate: health, sign-in, the launcher's static files
/// (logos, fonts), /admin, which checks for the admin role itself, and web app manifests, which
/// browsers fetch without cookies (they describe the app: name, icons, colors, start URL).
fn open_path(path: &str) -> bool {
    path == "/healthz"
        || path == "/admin"
        || ["/readyz/", "/auth/", "/launcher/", "/admin/"]
            .iter()
            .any(|p| path.starts_with(p))
        || path.ends_with(".webmanifest")
}

/// With `[auth] apps = "signed-in"`, the launcher, status and every app need an account with the
/// user or admin role. Page loads go to the sign-in page; other requests get 401.
pub async fn apps_gate(
    State(s): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response<Body> {
    if s.cfg.auth.apps == AppsAccess::Public || open_path(req.uri().path()) {
        return next.run(req).await;
    }
    let c = ctx(&s, req.headers(), peer);
    match authenticate(&s, req.headers(), &c) {
        Ok(_) => next.run(req).await,
        Err(Denied::Login) if !is_navigation(req.headers()) => {
            simple(StatusCode::UNAUTHORIZED, "sign-in required\n")
        }
        Err(d) => {
            let mut res = denied_page(&s, d, req.uri().path(), req.uri().query());
            res.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            res
        }
    }
}

fn page(body: &'static str, content_type: &'static str) -> Response<Body> {
    let mut res = Response::new(Body::from(body));
    res.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    set_common_headers(&mut res);
    res.headers_mut()
        .insert("x-frame-options", HeaderValue::from_static("DENY"));
    res
}

fn json(status: StatusCode, value: serde_json::Value) -> Response<Body> {
    let mut res = (status, Json(value)).into_response();
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

fn not_found() -> Response<Body> {
    simple(StatusCode::NOT_FOUND, "not found\n")
}

fn forbidden(message: &str) -> Response<Body> {
    let mut r = simple(StatusCode::FORBIDDEN, "forbidden\n");
    *r.body_mut() = Body::from(format!("forbidden: {message}\n"));
    r
}

fn set_session(res: &mut Response<Body>, s: &Shared, c: &Ctx, token: &str, max_age: u64) {
    let v = session_cookie(token, max_age, cookie_secure(&s.cfg.auth, c.https));
    res.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&v).expect("cookie is ASCII"),
    );
}

fn start_session(s: &Shared, user: &User, method: &'static str) -> String {
    s.users.touch_login(user.id);
    s.sessions.create(
        SessionRef {
            user_id: user.id,
            version: user.session_version,
            method,
        },
        s.cfg.auth.session_ttl_secs,
    )
}

pub async fn login_page(State(s): State<Arc<Shared>>) -> Response<Body> {
    if !s.cfg.auth.sign_in.has_sessions() {
        return not_found();
    }
    page(LOGIN_HTML, "text/html; charset=utf-8")
}

pub async fn style() -> Response<Body> {
    page(LOGIN_CSS, "text/css; charset=utf-8")
}

/// What the sign-in page offers.
pub async fn options(State(s): State<Arc<Shared>>) -> Response<Body> {
    let sign_in = s.cfg.auth.sign_in;
    let oidc = s
        .oidc
        .as_ref()
        .filter(|_| sign_in.oidc())
        .map(|o| o.settings().name.clone());
    json(
        StatusCode::OK,
        serde_json::json!({ "local": sign_in.local(), "oidc": oidc }),
    )
}

#[derive(Deserialize)]
pub struct LoginForm {
    user: String,
    password: String,
    next: Option<String>,
}

pub async fn login_submit(
    State(s): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response<Body> {
    if !s.cfg.auth.sign_in.local() {
        return not_found();
    }
    let c = ctx(&s, &headers, peer);
    if !same_origin(&headers, c.host.as_deref()) {
        return simple(StatusCode::FORBIDDEN, "forbidden: cross-site login\n");
    }
    let next = safe_next(form.next.as_deref());
    if s.throttle.blocked(c.peer) {
        log::warn!("auth: sign-in from {} refused: too many failures", c.peer);
        return redirect(
            StatusCode::SEE_OTHER,
            &format!("login?failed=throttled&next={}", query_escape(&next)),
        );
    }
    let shared = s.clone();
    let (user, password) = (form.user.clone(), form.password);
    let found =
        tokio::task::spawn_blocking(move || shared.users.verify_password(&user, &password)).await;
    let account = match found {
        Ok(Ok(Some(u))) => u,
        Ok(Ok(None)) => {
            s.throttle.fail(c.peer);
            log::warn!("auth: failed sign-in for {:?} from {}", form.user, c.peer);
            tokio::time::sleep(Duration::from_secs(1)).await;
            return redirect(
                StatusCode::SEE_OTHER,
                &format!("login?failed=1&next={}", query_escape(&next)),
            );
        }
        Ok(Err(e)) => {
            log::error!("auth: user database: {e}");
            return simple(
                StatusCode::SERVICE_UNAVAILABLE,
                "user database unavailable\n",
            );
        }
        Err(_) => return simple(StatusCode::INTERNAL_SERVER_ERROR, "internal error\n"),
    };
    s.throttle.succeed(c.peer);
    log::info!(
        "auth: {} ({}) signed in (local) from {}",
        account.username,
        account.role.name(),
        c.peer
    );
    let token = start_session(&s, &account, "local");
    let mut res = redirect(StatusCode::SEE_OTHER, &format!("../{next}"));
    set_session(&mut res, &s, &c, &token, s.cfg.auth.session_ttl_secs);
    res
}

/// Ends the session. Same-origin only (the session cookie is `SameSite=Lax`).
pub async fn logout(
    State(s): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response<Body> {
    let c = ctx(&s, &headers, peer);
    if !same_origin(&headers, c.host.as_deref()) {
        return json(
            StatusCode::FORBIDDEN,
            serde_json::json!({ "error": "cross-site request" }),
        );
    }
    if let Some(t) = cookie(&headers, SESSION_COOKIE) {
        s.sessions.remove(&t);
    }
    let mut res = json(StatusCode::OK, serde_json::json!({ "signed_out": true }));
    set_session(&mut res, &s, &c, "", 0);
    res
}

fn oidc_state_cookie(value: &str, max_age: u64, secure: bool) -> HeaderValue {
    let mut c = format!(
        "{OIDC_STATE_COOKIE}={value}; Path=/auth/oidc; SameSite=Lax; HttpOnly; Max-Age={max_age}"
    );
    if secure {
        c.push_str("; Secure");
    }
    HeaderValue::from_str(&c).expect("cookie is ASCII")
}

#[derive(Deserialize)]
pub struct OidcStart {
    next: Option<String>,
    /// `1`: link the provider identity to the signed-in account.
    link: Option<String>,
}

pub async fn oidc_login(
    State(s): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<OidcStart>,
) -> Response<Body> {
    if s.oidc.is_none() || !s.cfg.auth.sign_in.oidc() {
        return not_found();
    }
    let c = ctx(&s, &headers, peer);
    // The state cookie and the session must be set on the host the provider returns to. A
    // sign-in started on another name for this server (e.g. a LAN alias) continues there.
    if let Some(canonical) = s
        .oidc
        .as_ref()
        .and_then(|o| callback_origin(&o.settings().redirect_url))
        && c.host
            .as_deref()
            .is_some_and(|h| !h.eq_ignore_ascii_case(&canonical.1))
    {
        let query = q
            .next
            .as_deref()
            .map(|n| format!("next={}", query_escape(&safe_next(Some(n)))));
        let link = q
            .link
            .as_deref()
            .filter(|l| *l == "1")
            .map(|_| "link=1".to_string());
        let params: Vec<String> = query.into_iter().chain(link).collect();
        let target = format!(
            "{}/auth/oidc/login{}",
            canonical.0,
            if params.is_empty() {
                String::new()
            } else {
                format!("?{}", params.join("&"))
            }
        );
        return redirect(StatusCode::SEE_OTHER, &target);
    }
    let next = safe_next(q.next.as_deref());
    let link = if q.link.as_deref() == Some("1") {
        match authenticate(&s, &headers, &c) {
            Ok(Principal {
                user_id: Some(id), ..
            }) => Some(id),
            _ => return forbidden("sign in first to link an account"),
        }
    } else {
        None
    };
    let shared = s.clone();
    match tokio::task::spawn_blocking(move || shared.oidc.as_ref().map(|o| o.start(next, link)))
        .await
    {
        Ok(Some(Ok((url, state)))) => {
            let mut res = redirect(StatusCode::SEE_OTHER, &url);
            res.headers_mut().append(
                header::SET_COOKIE,
                oidc_state_cookie(&state, 600, cookie_secure(&s.cfg.auth, c.https)),
            );
            res
        }
        Ok(Some(Err(e))) => {
            log::error!("auth: OIDC login cannot start: {e:#}");
            simple(StatusCode::BAD_GATEWAY, "identity provider unavailable\n")
        }
        _ => simple(StatusCode::INTERNAL_SERVER_ERROR, "internal error\n"),
    }
}

/// `(origin, host)` of the configured callback URL, e.g. `("https://apps.example.net",
/// "apps.example.net")`. The host keeps an explicit port, like the `Host` header does.
fn callback_origin(redirect_url: &str) -> Option<(String, String)> {
    let (scheme, rest) = redirect_url.split_once("://")?;
    let host = rest.split('/').next().filter(|h| !h.is_empty())?;
    Some((format!("{scheme}://{host}"), host.to_ascii_lowercase()))
}

#[derive(Deserialize)]
pub struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

pub async fn oidc_callback(
    State(s): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<Callback>,
) -> Response<Body> {
    let Some(oidc) = s.oidc.as_ref().filter(|_| s.cfg.auth.sign_in.oidc()) else {
        return not_found();
    };
    let c = ctx(&s, &headers, peer);
    let (Some(code), Some(state)) = (q.code, q.state) else {
        log::warn!("auth: OIDC callback without code ({:?})", q.error);
        return forbidden("login was not completed");
    };
    if cookie(&headers, OIDC_STATE_COOKIE).as_deref() != Some(state.as_str()) {
        log::warn!(
            "auth: OIDC callback from {} not started in this browser",
            c.peer
        );
        return forbidden("this sign-in was not started in this browser; start again");
    }
    let shared = s.clone();
    let result =
        tokio::task::spawn_blocking(move || shared.oidc.as_ref().map(|o| o.finish(&code, &state)))
            .await;
    let (identity, next, link) = match result {
        Ok(Some(Ok(r))) => r,
        Ok(Some(Err(e))) => {
            log::warn!("auth: OIDC login rejected from {}: {e:#}", c.peer);
            return forbidden("login could not be verified");
        }
        _ => return simple(StatusCode::INTERNAL_SERVER_ERROR, "internal error\n"),
    };
    let settings = oidc.settings();
    let clear_state = oidc_state_cookie("", 0, cookie_secure(&s.cfg.auth, c.https));

    // Linking: the identity goes to the account that started the flow, which must still be the
    // one signed in here.
    if let Some(uid) = link {
        let current = authenticate(&s, &headers, &c).ok().and_then(|p| p.user_id);
        if current != Some(uid) {
            return forbidden("sign in again to link this account");
        }
        return match s.users.link_oidc(uid, &settings.issuer, &identity.subject) {
            Ok(u) => {
                log::info!(
                    "auth: {} linked identity {} at {}",
                    u.username,
                    identity.subject,
                    settings.issuer
                );
                let mut res = redirect(StatusCode::SEE_OTHER, &format!("../../{next}"));
                res.headers_mut().append(header::SET_COOKIE, clear_state);
                res
            }
            Err(UserError::Conflict(_)) => {
                forbidden("this identity is already linked to another account")
            }
            Err(e) => {
                log::error!("auth: link failed: {e}");
                simple(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "user database unavailable\n",
                )
            }
        };
    }

    let auth = &s.cfg.auth;
    let provider_role = if auth.provider_roles() {
        match auth.role_for(&identity.user, &identity.groups) {
            Some(r) => Some(r),
            None => {
                log::warn!("auth: OIDC user {} has no role", identity.user);
                return forbidden("this account has no access");
            }
        }
    } else {
        None
    };
    let claims = OidcClaims {
        issuer: &settings.issuer,
        subject: &identity.subject,
        username: &identity.user,
        email: identity.email.as_deref(),
        email_verified: identity.email_verified,
    };
    let account = match s.users.oidc_sign_in(
        &claims,
        provider_role,
        settings.link_by_email,
        settings.create_users,
    ) {
        Ok(OidcOutcome::Existing(u)) => u,
        Ok(OidcOutcome::Linked(u)) => {
            log::info!(
                "auth: linked identity {} to {} by verified e-mail",
                identity.subject,
                u.username
            );
            u
        }
        Ok(OidcOutcome::Created(u)) => {
            log::info!(
                "auth: created {} ({}) for identity {}",
                u.username,
                u.role.name(),
                identity.subject
            );
            u
        }
        Ok(OidcOutcome::NoAccount) => {
            log::warn!(
                "auth: no account for OIDC identity {} ({})",
                identity.subject,
                identity.user
            );
            return forbidden("there is no account for this identity; ask an administrator");
        }
        Err(e) => {
            log::error!("auth: OIDC account: {e}");
            return simple(
                StatusCode::SERVICE_UNAVAILABLE,
                "user database unavailable\n",
            );
        }
    };
    log::info!(
        "auth: {} ({}) signed in (oidc) from {}",
        account.username,
        account.role.name(),
        c.peer
    );
    let token = start_session(&s, &account, "oidc");
    let mut res = redirect(StatusCode::SEE_OTHER, &format!("../../{next}"));
    set_session(&mut res, &s, &c, &token, auth.session_ttl_secs);
    res.headers_mut().append(header::SET_COOKIE, clear_state);
    res
}

#[derive(Serialize)]
struct Me {
    user: String,
    role: &'static str,
    method: &'static str,
    can_logout: bool,
    /// Site-relative URL of the admin interface when this account may use it here.
    admin_url: Option<String>,
    /// Site-relative URL that links the provider identity to this account, when possible.
    link_url: Option<String>,
    link_name: Option<String>,
}

/// Who is signed in, for the launcher. 401 when nobody is.
pub async fn me(
    State(s): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response<Body> {
    let c = ctx(&s, &headers, peer);
    match authenticate(&s, &headers, &c) {
        Ok(p) => {
            let admin_url =
                (p.role == Role::Admin && s.cfg.admin.enabled && s.cfg.admin.host.is_none())
                    .then(|| "admin/".to_string());
            let unlinked = p
                .user_id
                .and_then(|id| s.users.get(id).ok().flatten())
                .is_some_and(|u| u.oidc.is_none());
            let link_name = s
                .oidc
                .as_ref()
                .filter(|_| unlinked && s.cfg.auth.sign_in.oidc())
                .map(|o| o.settings().name.clone());
            let me = Me {
                can_logout: s.cfg.auth.sign_in.has_sessions(),
                link_url: link_name
                    .as_ref()
                    .map(|_| "auth/oidc/login?link=1&next=".to_string()),
                link_name,
                user: p.user,
                role: p.role.name(),
                method: p.method,
                admin_url,
            };
            json(StatusCode::OK, serde_json::to_value(me).unwrap_or_default())
        }
        Err(_) => json(
            StatusCode::UNAUTHORIZED,
            serde_json::json!({ "error": "not signed in" }),
        ),
    }
}

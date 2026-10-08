//! Sign-in for the whole site, per `[auth]`: who the requester is ([`authenticate`]), the `/auth/`
//! endpoints (login form, OIDC, logout, `me`) and the gate in front of the launcher and the apps
//! when `[auth] apps = "signed-in"`.
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

use crate::access::{AppsAccess, AuthMethod, Role};
use crate::auth::{
    Principal, SESSION_COOKIE, basic_credentials, client_is_https, cookie, cookie_secure,
    proxy_identity, query_escape, request_host, safe_next, same_origin, session_cookie, to_root,
};
use crate::serve::{redirect, set_common_headers, simple};
use crate::server::Shared;

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
    /// Not signed in; form and OIDC can fix that with a login page.
    Login,
    /// Basic: ask the browser for credentials.
    Challenge,
    Forbidden(&'static str),
}

/// The signed-in account, if any. With `method = "none"` everyone is an anonymous admin.
pub fn authenticate(s: &Shared, headers: &HeaderMap, c: &Ctx) -> Result<Principal, Denied> {
    let auth = &s.cfg.auth;
    match auth.method {
        AuthMethod::None => Ok(Principal {
            user: "anonymous".into(),
            method: "none",
            role: Role::Admin,
        }),
        AuthMethod::Basic => match basic_credentials(headers) {
            Some((u, p)) => match s.users.verify(&u, &p) {
                Some(role) => Ok(Principal {
                    user: u,
                    method: "basic",
                    role,
                }),
                None => Err(Denied::Challenge),
            },
            None => Err(Denied::Challenge),
        },
        AuthMethod::Form | AuthMethod::Oidc => cookie(headers, SESSION_COOKIE)
            .and_then(|t| s.sessions.get(&t))
            .ok_or(Denied::Login),
        AuthMethod::Proxy => match proxy_identity(headers, c.peer, &s.cfg.server, auth) {
            Some((u, g)) => match auth.role_for(&u, &g) {
                Some(role) => Ok(Principal {
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
            let page = if s.cfg.auth.method == AuthMethod::Oidc {
                "auth/oidc/login"
            } else {
                "auth/login"
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
/// browsers fetch without cookies (they only describe the app: name, icons, colors).
fn open_path(path: &str) -> bool {
    path == "/healthz"
        || path == "/admin"
        || ["/readyz/", "/auth/", "/launcher/", "/admin/"]
            .iter()
            .any(|p| path.starts_with(p))
        || path.ends_with(".webmanifest")
}

/// With `[auth] apps = "signed-in"`, the launcher, status and every app need an account with the
/// user or admin role. Page loads go to the login page; other requests get 401.
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

fn set_session(res: &mut Response<Body>, s: &Shared, c: &Ctx, token: &str, max_age: u64) {
    let v = session_cookie(token, max_age, cookie_secure(&s.cfg.auth, c.https));
    res.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&v).expect("cookie is ASCII"),
    );
}

pub async fn login_page(State(s): State<Arc<Shared>>) -> Response<Body> {
    if s.cfg.auth.method != AuthMethod::Form {
        return not_found();
    }
    page(LOGIN_HTML, "text/html; charset=utf-8")
}

pub async fn style() -> Response<Body> {
    page(LOGIN_CSS, "text/css; charset=utf-8")
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
    if s.cfg.auth.method != AuthMethod::Form {
        return not_found();
    }
    let c = ctx(&s, &headers, peer);
    if !same_origin(&headers, c.host.as_deref()) {
        return simple(StatusCode::FORBIDDEN, "forbidden: cross-site login\n");
    }
    let next = safe_next(form.next.as_deref());
    let shared = s.clone();
    let (user, password) = (form.user.clone(), form.password);
    let role = tokio::task::spawn_blocking(move || shared.users.verify(&user, &password))
        .await
        .ok()
        .flatten();
    let Some(role) = role else {
        log::warn!("auth: failed login for {:?} from {}", form.user, c.peer);
        tokio::time::sleep(Duration::from_secs(1)).await;
        return redirect(
            StatusCode::SEE_OTHER,
            &format!("login?failed=1&next={}", query_escape(&next)),
        );
    };
    log::info!(
        "auth: {} ({}) signed in (form) from {}",
        form.user,
        role.name(),
        c.peer
    );
    let ttl = s.cfg.auth.session_ttl_secs;
    let token = s.sessions.create(
        Principal {
            user: form.user,
            method: "form",
            role,
        },
        ttl,
    );
    let mut res = redirect(StatusCode::SEE_OTHER, &format!("../{next}"));
    set_session(&mut res, &s, &c, &token, ttl);
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

#[derive(Deserialize)]
pub struct NextQuery {
    next: Option<String>,
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

pub async fn oidc_login(
    State(s): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<NextQuery>,
) -> Response<Body> {
    if s.oidc.is_none() || s.cfg.auth.method != AuthMethod::Oidc {
        return not_found();
    }
    let c = ctx(&s, &headers, peer);
    let next = safe_next(q.next.as_deref());
    let shared = s.clone();
    match tokio::task::spawn_blocking(move || shared.oidc.as_ref().map(|o| o.start(next))).await {
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
    if s.cfg.auth.method != AuthMethod::Oidc || s.oidc.is_none() {
        return not_found();
    }
    let c = ctx(&s, &headers, peer);
    let (Some(code), Some(state)) = (q.code, q.state) else {
        log::warn!("auth: OIDC callback without code ({:?})", q.error);
        return simple(
            StatusCode::FORBIDDEN,
            "forbidden: login was not completed\n",
        );
    };
    if cookie(&headers, OIDC_STATE_COOKIE).as_deref() != Some(state.as_str()) {
        log::warn!(
            "auth: OIDC callback from {} not started in this browser",
            c.peer
        );
        return simple(
            StatusCode::FORBIDDEN,
            "forbidden: this sign-in was not started in this browser; start again\n",
        );
    }
    let shared = s.clone();
    let result =
        tokio::task::spawn_blocking(move || shared.oidc.as_ref().map(|o| o.finish(&code, &state)))
            .await;
    let (identity, next) = match result {
        Ok(Some(Ok(r))) => r,
        Ok(Some(Err(e))) => {
            log::warn!("auth: OIDC login rejected from {}: {e:#}", c.peer);
            return simple(
                StatusCode::FORBIDDEN,
                "forbidden: login could not be verified\n",
            );
        }
        _ => return simple(StatusCode::INTERNAL_SERVER_ERROR, "internal error\n"),
    };
    let Some(role) = s.cfg.auth.role_for(&identity.user, &identity.groups) else {
        log::warn!("auth: OIDC user {} has no role", identity.user);
        return simple(
            StatusCode::FORBIDDEN,
            "forbidden: this account has no access\n",
        );
    };
    log::info!(
        "auth: {} ({}) signed in (oidc) from {}",
        identity.user,
        role.name(),
        c.peer
    );
    let ttl = s.cfg.auth.session_ttl_secs;
    let token = s.sessions.create(
        Principal {
            user: identity.user,
            method: "oidc",
            role,
        },
        ttl,
    );
    let mut res = redirect(StatusCode::SEE_OTHER, &format!("../../{next}"));
    set_session(&mut res, &s, &c, &token, ttl);
    res.headers_mut().append(
        header::SET_COOKIE,
        oidc_state_cookie("", 0, cookie_secure(&s.cfg.auth, c.https)),
    );
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
            let me = Me {
                can_logout: s.cfg.auth.method.has_sessions(),
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

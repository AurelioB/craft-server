//! The `/admin` interface for accounts with the admin role (see `login.rs` for sign-in): a status
//! API and update actions (check, update, apply, pin, unpin, rollback, allow) run one at a time in
//! the background.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode, header};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use crate::access::{Role, SignIn};
use crate::auth::{CSRF_COOKIE, Principal, cookie, cookie_secure, csrf_cookie, csrf_ok};
use crate::fsutil::random_hex;
use crate::login::{Ctx, Denied, authenticate, ctx, denied_page};
use crate::ops::{Outcome, Updater};
use crate::serve::{redirect, set_common_headers, simple};
use crate::server::Shared;
use crate::status;
use crate::store::{HistoryEntry, Store};
use crate::timeutil::now_iso;
use crate::users::{MIN_PASSWORD_LEN, NewUser, User, UserError, UserUpdate};

const ADMIN_HTML: &str = include_str!("../assets/admin.html");
const ADMIN_JS: &str = include_str!("../assets/admin.js");
const ADMIN_CSS: &str = include_str!("../assets/admin.css");
const MAX_RESULTS: usize = 20;

#[derive(Clone, Serialize)]
pub struct JobResult {
    at: String,
    app: String,
    action: String,
    user: String,
    ok: bool,
    message: String,
}

#[derive(Default)]
pub struct Jobs {
    running: parking_lot::Mutex<Option<String>>,
    results: parking_lot::Mutex<VecDeque<JobResult>>,
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

fn json_error(status: StatusCode, message: &str) -> Response<Body> {
    let mut res = Response::new(Body::from(
        serde_json::json!({ "error": message }).to_string(),
    ));
    *res.status_mut() = status;
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

/// The signed-in administrator, or why not.
fn authorize(s: &Shared, headers: &HeaderMap, c: &Ctx) -> Result<Principal, Denied> {
    let p = authenticate(s, headers, c)?;
    if p.role == Role::Admin {
        Ok(p)
    } else {
        Err(Denied::Forbidden("this account may not manage updates"))
    }
}

fn denied_api(d: Denied) -> Response<Body> {
    match d {
        Denied::Challenge => denied_page_basic(),
        Denied::Login => json_error(StatusCode::UNAUTHORIZED, "login required"),
        Denied::Forbidden(m) => json_error(StatusCode::FORBIDDEN, m),
    }
}

fn denied_page_basic() -> Response<Body> {
    let mut r = simple(StatusCode::UNAUTHORIZED, "authentication required\n");
    r.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"Craft Apps\", charset=\"UTF-8\""),
    );
    r
}

/// `/admin` without a trailing slash.
pub async fn root_redirect() -> Response<Body> {
    redirect(StatusCode::MOVED_PERMANENTLY, "admin/")
}

pub async fn index(
    State(s): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response<Body> {
    let c = ctx(&s, &headers, peer);
    if let Err(d) = authorize(&s, &headers, &c) {
        return denied_page(&s, d, "/admin/", None);
    }
    let mut res = page(ADMIN_HTML, "text/html; charset=utf-8");
    if cookie(&headers, CSRF_COOKIE).is_none_or(|v| v.len() < 32) {
        let secure = cookie_secure(&s.cfg.auth, c.https);
        if let Ok(v) = HeaderValue::from_str(&csrf_cookie(&random_hex(32), secure)) {
            res.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    res
}

pub async fn asset(Path(name): Path<String>) -> Response<Body> {
    match name.as_str() {
        "admin.js" => page(ADMIN_JS, "text/javascript; charset=utf-8"),
        "admin.css" => page(ADMIN_CSS, "text/css; charset=utf-8"),
        _ => simple(StatusCode::NOT_FOUND, "not found\n"),
    }
}

fn user_error(e: UserError) -> Response<Body> {
    let status = match &e {
        UserError::Invalid(_) => StatusCode::BAD_REQUEST,
        UserError::Conflict(_) | UserError::LastAdmin => StatusCode::CONFLICT,
        UserError::NotFound => StatusCode::NOT_FOUND,
        UserError::Db(m) => {
            log::error!("admin: user database: {m}");
            StatusCode::SERVICE_UNAVAILABLE
        }
    };
    json_error(status, &e.to_string())
}

fn no_store_json(status: StatusCode, value: impl Serialize) -> Response<Body> {
    let mut res = (status, Json(value)).into_response();
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

/// An administrator allowed to change accounts: signed in with the admin role, and the request
/// carries the CSRF token from the same origin.
fn authorize_change(
    s: &Shared,
    headers: &HeaderMap,
    c: &Ctx,
) -> Result<Principal, Box<Response<Body>>> {
    let p = authorize(s, headers, c).map_err(|d| Box::new(denied_api(d)))?;
    if !csrf_ok(headers, c.host.as_deref()) {
        return Err(Box::new(json_error(
            StatusCode::FORBIDDEN,
            "missing or invalid CSRF token",
        )));
    }
    if !s.cfg.auth.sign_in.uses_accounts() {
        return Err(Box::new(json_error(
            StatusCode::CONFLICT,
            "accounts are not used with this sign-in method",
        )));
    }
    Ok(p)
}

#[derive(Serialize)]
struct UsersPage {
    users: Vec<User>,
    /// The signed-in administrator's account, if it is one.
    me: Option<i64>,
    local: bool,
    oidc: Option<String>,
    /// Roles of OIDC accounts follow the provider's groups at every sign-in.
    provider_roles: bool,
    /// Whether this sign-in method uses these accounts at all.
    accounts_used: bool,
    min_password: usize,
}

pub async fn users_list(
    State(s): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response<Body> {
    let c = ctx(&s, &headers, peer);
    let p = match authorize(&s, &headers, &c) {
        Ok(p) => p,
        Err(d) => return denied_api(d),
    };
    let sign_in = s.cfg.auth.sign_in;
    match s.users.list() {
        Ok(users) => no_store_json(
            StatusCode::OK,
            UsersPage {
                users,
                me: p.user_id,
                local: sign_in.local() || sign_in == SignIn::Basic,
                oidc: s
                    .oidc
                    .as_ref()
                    .filter(|_| sign_in.oidc())
                    .map(|o| o.settings().name.clone()),
                provider_roles: sign_in.oidc() && s.cfg.auth.provider_roles(),
                accounts_used: sign_in.uses_accounts(),
                min_password: MIN_PASSWORD_LEN,
            },
        ),
        Err(e) => user_error(e),
    }
}

#[derive(Deserialize)]
pub struct CreateBody {
    username: String,
    email: Option<String>,
    password: Option<String>,
    role: String,
}

fn bad_role() -> Response<Body> {
    json_error(StatusCode::BAD_REQUEST, "role must be user or admin")
}

pub async fn users_create(
    State(s): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<CreateBody>,
) -> Response<Body> {
    let c = ctx(&s, &headers, peer);
    let p = match authorize_change(&s, &headers, &c) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let Some(role) = Role::parse(&body.role) else {
        return bad_role();
    };
    let shared = s.clone();
    let created = tokio::task::spawn_blocking(move || {
        shared.users.create(NewUser {
            username: &body.username,
            email: body.email.as_deref(),
            password: body.password.as_deref().filter(|p| !p.is_empty()),
            role,
        })
    })
    .await;
    match created {
        Ok(Ok(u)) => {
            log::info!(
                "admin: {} created user {} ({})",
                p.user,
                u.username,
                u.role.name()
            );
            no_store_json(StatusCode::CREATED, u)
        }
        Ok(Err(e)) => user_error(e),
        Err(_) => json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
    }
}

#[derive(Deserialize)]
pub struct UpdateBody {
    /// Absent: unchanged; empty string: remove the address.
    email: Option<String>,
    role: Option<String>,
    /// Absent or empty: unchanged.
    password: Option<String>,
}

pub async fn users_update(
    State(s): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(body): Json<UpdateBody>,
) -> Response<Body> {
    let c = ctx(&s, &headers, peer);
    let p = match authorize_change(&s, &headers, &c) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let role = match body.role.as_deref() {
        None => None,
        Some(text) => match Role::parse(text) {
            Some(r) => Some(r),
            None => return bad_role(),
        },
    };
    let shared = s.clone();
    let updated = tokio::task::spawn_blocking(move || {
        shared.users.update(
            id,
            UserUpdate {
                email: body
                    .email
                    .as_deref()
                    .map(|e| Some(e).filter(|e| !e.is_empty())),
                role,
                password: body.password.as_deref().filter(|p| !p.is_empty()),
            },
        )
    })
    .await;
    match updated {
        Ok(Ok(u)) => {
            log::info!("admin: {} updated user {}", p.user, u.username);
            no_store_json(StatusCode::OK, u)
        }
        Ok(Err(e)) => user_error(e),
        Err(_) => json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
    }
}

/// `delete` or `unlink` (remove the linked OIDC identity).
pub async fn users_action(
    State(s): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((id, action)): Path<(i64, String)>,
    headers: HeaderMap,
) -> Response<Body> {
    let c = ctx(&s, &headers, peer);
    let p = match authorize_change(&s, &headers, &c) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let target = match s.users.get(id) {
        Ok(Some(u)) => u,
        Ok(None) => return user_error(UserError::NotFound),
        Err(e) => return user_error(e),
    };
    let result = match action.as_str() {
        "delete" if p.user_id == Some(id) => {
            return json_error(StatusCode::CONFLICT, "you cannot delete your own account");
        }
        "delete" => s.users.delete(id).map(|()| None),
        "unlink" => s.users.unlink_oidc(id).map(Some),
        _ => return json_error(StatusCode::NOT_FOUND, "unknown action"),
    };
    match result {
        Ok(u) => {
            s.sessions.remove_user(id);
            log::info!("admin: {} did {action} on user {}", p.user, target.username);
            no_store_json(StatusCode::OK, serde_json::json!({ "ok": true, "user": u }))
        }
        Err(e) => user_error(e),
    }
}

#[derive(Serialize)]
struct AdminStatus {
    user: String,
    auth: &'static str,
    can_logout: bool,
    running: Option<String>,
    results: Vec<JobResult>,
    history: Vec<HistoryEntry>,
    status: status::Status,
}

pub async fn api_status(
    State(s): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response<Body> {
    let c = ctx(&s, &headers, peer);
    let principal = match authorize(&s, &headers, &c) {
        Ok(p) => p,
        Err(d) => return denied_api(d),
    };
    let shared = s.clone();
    let body = tokio::task::spawn_blocking(move || {
        let store = Store::new(&shared.cfg.paths.state);
        AdminStatus {
            user: principal.user,
            auth: principal.method,
            can_logout: shared.cfg.auth.sign_in.has_sessions(),
            running: shared.jobs.running.lock().clone(),
            results: shared.jobs.results.lock().iter().cloned().collect(),
            history: store.history(None, 40),
            status: status::build(&shared.cfg, &store, false),
        }
    })
    .await;
    match body {
        Ok(b) => {
            let mut res = Json(b).into_response();
            res.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            res
        }
        Err(_) => json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
    }
}

#[derive(Deserialize, Default)]
pub struct ActionBody {
    version: Option<String>,
}

const ACTIONS: &[&str] = &[
    "check", "update", "apply", "pin", "unpin", "rollback", "allow",
];

pub async fn api_action(
    State(s): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((app_id, action)): Path<(String, String)>,
    headers: HeaderMap,
    body: Option<Json<ActionBody>>,
) -> Response<Body> {
    let c = ctx(&s, &headers, peer);
    let principal = match authorize(&s, &headers, &c) {
        Ok(p) => p,
        Err(d) => return denied_api(d),
    };
    if !csrf_ok(&headers, c.host.as_deref()) {
        return json_error(StatusCode::FORBIDDEN, "missing or invalid CSRF token");
    }
    if !ACTIONS.contains(&action.as_str()) {
        return json_error(StatusCode::NOT_FOUND, "unknown action");
    }
    let Ok(app) = s.cfg.app(&app_id) else {
        return json_error(StatusCode::NOT_FOUND, "unknown app");
    };
    if !app.enabled {
        return json_error(StatusCode::CONFLICT, "app is disabled in config.toml");
    }
    let version = body
        .and_then(|b| b.0.version)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    if matches!(action.as_str(), "pin" | "allow") && version.is_none() {
        return json_error(StatusCode::BAD_REQUEST, "this action needs a version");
    }
    {
        let mut running = s.jobs.running.lock();
        if let Some(r) = running.as_ref() {
            return json_error(StatusCode::CONFLICT, &format!("busy: {r}"));
        }
        *running = Some(format!("{action} {app_id}"));
    }
    log::info!(
        "admin: {} requested {action} {app_id} {}",
        principal.user,
        version.as_deref().unwrap_or("")
    );
    let shared = s.clone();
    std::thread::spawn(move || {
        let (ok, message) = run_action(
            &shared,
            &app_id,
            &action,
            version.as_deref(),
            &principal.user,
        );
        let mut results = shared.jobs.results.lock();
        results.push_front(JobResult {
            at: now_iso(),
            app: app_id,
            action,
            user: principal.user,
            ok,
            message,
        });
        results.truncate(MAX_RESULTS);
        *shared.jobs.running.lock() = None;
    });
    let mut res = json_error(StatusCode::ACCEPTED, "started");
    *res.body_mut() = Body::from(r#"{"started":true}"#);
    res
}

fn run_action(
    s: &Shared,
    app_id: &str,
    action: &str,
    version: Option<&str>,
    user: &str,
) -> (bool, String) {
    let cfg = &s.cfg;
    let Ok(app) = cfg.app(app_id) else {
        return (false, "unknown app".into());
    };
    let updater = Updater::new(cfg).with_activity(s.activity.clone());
    let _lock = match updater.store.lock(
        Duration::from_secs(cfg.updater.lock_wait_secs),
        &format!("admin {user}: {action} {app_id}"),
    ) {
        Ok(l) => l,
        Err(e) => return (false, format!("{e:#}")),
    };
    let _ = updater.store.record(
        app_id,
        "admin",
        "requested",
        version,
        &format!("{action} by {user}"),
    );
    if let Err(e) = updater.prepare() {
        return (false, format!("{e:#}"));
    }
    let outcome = |o: Outcome| (!matches!(o, Outcome::Failed(_)), o.to_string());
    let text = |r: anyhow::Result<String>| match r {
        Ok(m) => (true, m),
        Err(e) => (false, format!("{e:#}")),
    };
    match action {
        "check" => match updater.check(app) {
            Ok(r) => (
                true,
                format!(
                    "active {}, latest {}{}",
                    r.active.as_deref().unwrap_or("none"),
                    r.latest.as_deref().unwrap_or("none"),
                    if r.update_available {
                        " (update available)"
                    } else {
                        ""
                    }
                ),
            ),
            Err(e) => (false, format!("{e:#}")),
        },
        "update" => outcome(updater.update_app(app, true)),
        "apply" => match updater.apply_pending(app, true) {
            Ok(Some(v)) => (true, format!("activated {v}")),
            Ok(None) => (false, "no release is waiting for activation".into()),
            Err(e) => (false, format!("{e:#}")),
        },
        "pin" => match updater.pin(app, version.unwrap_or_default()) {
            Ok(o) => outcome(o),
            Err(e) => (false, format!("{e:#}")),
        },
        "unpin" => text(updater.unpin(app)),
        "rollback" => text(updater.rollback(app, version)),
        "allow" => text(updater.allow(app, version.unwrap_or_default())),
        _ => (false, "unknown action".into()),
    }
}

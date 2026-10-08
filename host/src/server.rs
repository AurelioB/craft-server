//! The HTTP server: launcher, `/status.json`, health endpoints, the admin interface and app
//! releases under stable per-app paths.
//!
//! `/<entry>/` redirects (relative 302) to the active immutable release `/<entry>/<version>/`;
//! tabs that loaded a release keep requesting files from it after an update.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::{HeaderValue, Method, Response, StatusCode, header};
use axum::middleware::{self, Next};
use axum::routing::{get, post};

use crate::access::AdminAuth;
use crate::activity::Activity;
use crate::admin::{self, Jobs};
use crate::auth::{Sessions, Users, request_host};
use crate::config::{AppConfig, Config};
use crate::layout::{self, MARKER, valid_release_name};
use crate::oidc::Oidc;
use crate::serve::{CachePolicy, redirect, serve_dir, set_common_headers, simple};
use crate::status;
use crate::store::Store;

const LAUNCHER_HTML: &str = include_str!("../assets/index.html");
const LAUNCHER_JS: &str = include_str!("../assets/launcher.js");
const LAUNCHER_CSS: &str = include_str!("../assets/launcher.css");
const INSTALLING_HTML: &str = include_str!("../assets/installing.html");

pub struct Shared {
    pub cfg: Arc<Config>,
    pub activity: Arc<Activity>,
    pub users: Users,
    pub sessions: Sessions,
    pub oidc: Option<Oidc>,
    pub jobs: Jobs,
}

impl Shared {
    pub fn new(cfg: Arc<Config>, activity: Arc<Activity>) -> Arc<Self> {
        let oidc = cfg
            .admin
            .oidc
            .clone()
            .map(|o| Oidc::new(o, cfg.updater.http_timeout_secs));
        Arc::new(Self {
            users: Users::new(cfg.admin.users_file.clone()),
            sessions: Sessions::default(),
            oidc,
            jobs: Jobs::default(),
            cfg,
            activity,
        })
    }
}

fn static_page(body: &'static str, content_type: &'static str) -> Response<Body> {
    let mut res = Response::new(Body::from(body));
    res.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    set_common_headers(&mut res);
    res
}

async fn launcher() -> Response<Body> {
    static_page(LAUNCHER_HTML, "text/html; charset=utf-8")
}

async fn launcher_asset(Path(name): Path<String>) -> Response<Body> {
    match name.as_str() {
        "launcher.js" => static_page(LAUNCHER_JS, "text/javascript; charset=utf-8"),
        "launcher.css" => static_page(LAUNCHER_CSS, "text/css; charset=utf-8"),
        "installing.html" => static_page(INSTALLING_HTML, "text/html; charset=utf-8"),
        _ => simple(StatusCode::NOT_FOUND, "not found\n"),
    }
}

async fn healthz() -> Response<Body> {
    let mut r = simple(StatusCode::OK, "ok\n");
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn find_entry<'a>(cfg: &'a Config, entry: &str) -> Option<&'a AppConfig> {
    cfg.enabled_apps().find(|a| a.entry == entry)
}

async fn readyz(State(s): State<Arc<Shared>>, Path(entry): Path<String>) -> Response<Body> {
    match find_entry(&s.cfg, &entry) {
        Some(app) if layout::active_version(&s.cfg, app).is_some() => healthz().await,
        Some(_) => simple(StatusCode::SERVICE_UNAVAILABLE, "not ready\n"),
        None => simple(StatusCode::NOT_FOUND, "not found\n"),
    }
}

async fn status_json(State(s): State<Arc<Shared>>) -> Response<Body> {
    let shared = s.clone();
    let built = tokio::task::spawn_blocking(move || {
        serde_json::to_vec(&status::build(
            &shared.cfg,
            &Store::new(&shared.cfg.paths.state),
            true,
        ))
    })
    .await;
    let Ok(Ok(body)) = built else {
        return simple(StatusCode::INTERNAL_SERVER_ERROR, "internal error\n");
    };
    let mut res = Response::new(Body::from(body));
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    set_common_headers(&mut res);
    res
}

fn installing() -> Response<Body> {
    let mut res = static_page(INSTALLING_HTML, "text/html; charset=utf-8");
    *res.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    res
}

/// Everything under `/<entry>/…`.
async fn app_request(State(s): State<Arc<Shared>>, req: Request) -> Response<Body> {
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return simple(StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n");
    }
    let path = req.uri().path().to_string();
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let mut parts = path.trim_start_matches('/').splitn(3, '/');
    let entry = parts.next().unwrap_or("");
    let Some(app) = find_entry(&s.cfg, entry) else {
        return simple(StatusCode::NOT_FOUND, "not found\n");
    };
    let version = parts.next();
    let rest = parts.next();
    match (version, rest) {
        // /photocraft
        (None, _) => redirect(StatusCode::MOVED_PERMANENTLY, &format!("{entry}/{query}")),
        // /photocraft/
        (Some(""), None) => {
            s.activity.record(&app.id, None);
            match layout::active_version(&s.cfg, app) {
                Some(v) => redirect(StatusCode::FOUND, &format!("{v}/{query}")),
                None => installing(),
            }
        }
        // /photocraft/0.5.0
        (Some(v), None) => redirect(StatusCode::MOVED_PERMANENTLY, &format!("{v}/{query}")),
        // /photocraft/0.5.0/…
        (Some(v), Some(rest)) => {
            let root = s.cfg.release_root(app).join(v);
            if !valid_release_name(v) || !root.join(MARKER).is_file() {
                return simple(StatusCode::NOT_FOUND, "not found\n");
            }
            s.activity.record(&app.id, Some(v));
            let rest = format!("/{rest}{query}");
            serve_dir(root, &rest, req, CachePolicy::Immutable).await
        }
    }
}

/// With `[admin] host`, the admin interface lives only on that host name, and nothing else is
/// served there, so it never shares a browser origin with the apps.
async fn host_gate(
    State(s): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response<Body> {
    let path = req.uri().path();
    let is_admin = path == "/admin" || path.starts_with("/admin/");
    if let Some(admin_host) = &s.cfg.admin.host {
        let host = request_host(req.headers(), peer.ip(), &s.cfg.server);
        let on_admin_host = host.as_deref() == Some(admin_host.as_str());
        if is_admin != on_admin_host && path != "/healthz" {
            return simple(StatusCode::NOT_FOUND, "not found\n");
        }
    }
    if is_admin && s.cfg.admin.auth == AdminAuth::Disabled {
        return simple(StatusCode::NOT_FOUND, "not found\n");
    }
    next.run(req).await
}

pub fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/", get(launcher))
        .route("/launcher/{name}", get(launcher_asset))
        .route("/status.json", get(status_json))
        .route("/healthz", get(healthz))
        .route("/readyz/{entry}", get(readyz))
        .route("/admin", get(admin::root_redirect))
        .route("/admin/", get(admin::index))
        .route(
            "/admin/login",
            get(admin::login_page).post(admin::login_submit),
        )
        .route("/admin/logout", post(admin::logout))
        .route("/admin/oidc/login", get(admin::oidc_login))
        .route("/admin/oidc/callback", get(admin::oidc_callback))
        .route("/admin/api/status", get(admin::api_status))
        .route("/admin/api/apps/{app}/{action}", post(admin::api_action))
        .route("/admin/{name}", get(admin::asset))
        .fallback(app_request)
        .layer(middleware::from_fn_with_state(shared.clone(), host_gate))
        .with_state(shared)
}

/// Serve until the process is stopped.
pub async fn serve(shared: Arc<Shared>) -> anyhow::Result<()> {
    let addr = shared.cfg.server.listen;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    log::info!("listening on http://{addr}");
    let app = router(shared).into_make_service_with_connect_info::<SocketAddr>();
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("signal handler");
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        })
        .await?;
    Ok(())
}

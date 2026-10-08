//! Private loopback listener serving staged candidates with the production serving code, so a
//! release is tested over HTTP before it is published. Not reachable from outside the process's
//! network namespace (127.0.0.1, random port).

use std::collections::HashMap;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as UrlPath, Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use parking_lot::Mutex;

use crate::serve::{CachePolicy, serve_dir, simple};

static SERVERS: LazyLock<Mutex<HashMap<PathBuf, String>>> = LazyLock::new(Default::default);

fn valid_staging_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._+-".contains(&c))
}

async fn candidate(
    State(root): State<Arc<PathBuf>>,
    UrlPath((name, rest)): UrlPath<(String, String)>,
    req: Request,
) -> Response<Body> {
    if !valid_staging_name(&name) {
        return simple(StatusCode::NOT_FOUND, "not found\n");
    }
    serve_dir(
        root.join(name),
        &format!("/{rest}"),
        req,
        CachePolicy::NoStore,
    )
    .await
}

/// Base URL (ending in `/candidate/`) of the listener serving `staging_root`, starting it on first use.
pub fn base_url(staging_root: &Path) -> std::io::Result<String> {
    let mut servers = SERVERS.lock();
    if let Some(url) = servers.get(staging_root) {
        return Ok(url.clone());
    }
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let url = format!("http://{}/candidate/", listener.local_addr()?);
    let root = Arc::new(staging_root.to_path_buf());
    std::thread::Builder::new()
        .name("candidate-listener".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(async move {
                let app = Router::new()
                    .route("/candidate/{name}/{*rest}", get(candidate))
                    .with_state(root);
                let listener = tokio::net::TcpListener::from_std(listener).expect("listener");
                if let Err(e) = axum::serve(listener, app).await {
                    log::error!("candidate listener stopped: {e}");
                }
            });
        })?;
    servers.insert(staging_root.to_path_buf(), url.clone());
    Ok(url)
}

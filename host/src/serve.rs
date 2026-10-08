//! Static file serving for releases and staged candidates: precompressed copies, MIME types,
//! conditional requests, cache policy and hidden-file protection.

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{HeaderValue, Request, Response, StatusCode, Uri, header};
use tower::ServiceExt;
use tower_http::services::ServeDir;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CachePolicy {
    /// Versioned release URLs: content never changes.
    Immutable,
    /// Staged candidates.
    NoStore,
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Whether a request path (relative to the served root) may be served: no hidden segments
/// (release markers, `.htaccess`), no traversal, no backslashes or NUL, after decoding.
pub fn path_allowed(rest: &str) -> bool {
    let Some(decoded) = percent_decode(rest) else {
        return false;
    };
    !decoded.contains('\\')
        && !decoded.contains('\0')
        && decoded.split('/').all(|seg| !seg.starts_with('.'))
}

/// Entry pages, service workers and manifests revalidate even under versioned URLs, so a
/// browser never pairs a cached page with a different release layout.
fn revalidated(rest: &str) -> bool {
    rest.ends_with('/')
        || rest.ends_with("/index.html")
        || rest.ends_with("/sw.js")
        || rest.ends_with(".webmanifest")
}

pub fn set_common_headers(res: &mut Response<Body>) {
    let h = res.headers_mut();
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
}

pub fn simple(status: StatusCode, body: &'static str) -> Response<Body> {
    let mut res = Response::new(Body::from(body));
    *res.status_mut() = status;
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    set_common_headers(&mut res);
    res
}

/// Relative redirect, so the site works behind any proxy, host name or path prefix.
pub fn redirect(status: StatusCode, location: &str) -> Response<Body> {
    let mut res = Response::new(Body::empty());
    *res.status_mut() = status;
    if let Ok(v) = HeaderValue::from_str(location) {
        res.headers_mut().insert(header::LOCATION, v);
    }
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    set_common_headers(&mut res);
    res
}

/// Serve `rest` (a path starting with '/', optionally with a query) from `root`.
pub async fn serve_dir(
    root: PathBuf,
    rest: &str,
    req: Request<Body>,
    policy: CachePolicy,
) -> Response<Body> {
    let path = rest.split('?').next().unwrap_or(rest);
    if !path_allowed(path) {
        return simple(StatusCode::NOT_FOUND, "not found\n");
    }
    let (mut parts, body) = req.into_parts();
    parts.uri = match Uri::builder().path_and_query(rest).build() {
        Ok(u) => u,
        Err(_) => return simple(StatusCode::BAD_REQUEST, "bad request\n"),
    };
    let svc = ServeDir::new(root)
        .precompressed_br()
        .precompressed_gzip()
        .append_index_html_on_directories(true);
    let res = match svc.oneshot(Request::from_parts(parts, body)).await {
        Ok(r) => r.map(Body::new),
        Err(never) => match never {},
    };
    let mut res = res;
    let status = res.status();
    if status.is_redirection() && status != StatusCode::NOT_MODIFIED {
        // ServeDir adds a slash to directory paths using the rewritten path; make it relative.
        let last = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        return redirect(status, &format!("{last}/"));
    }
    let cache = match (
        policy,
        status.is_success() || status == StatusCode::NOT_MODIFIED,
    ) {
        (CachePolicy::NoStore, _) => "no-store",
        (CachePolicy::Immutable, true) if !revalidated(path) => {
            "public, max-age=31536000, immutable"
        }
        _ => "no-cache",
    };
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    set_common_headers(&mut res);
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_and_encoded_traversal_paths_are_refused() {
        assert!(path_allowed("/app.js"));
        assert!(path_allowed("/snippets/x/js/host.js"));
        assert!(!path_allowed("/.craft-release.json"));
        assert!(!path_allowed("/%2ecraft-release.json"));
        assert!(!path_allowed("/a/%2e%2e/b"));
        assert!(!path_allowed("/a/..%2fb"));
        assert!(!path_allowed("/a%5cb"));
        assert!(!path_allowed("/a%00b"));
        assert!(!path_allowed("/bad%zz"));
    }
}

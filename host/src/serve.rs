//! Static file serving for releases and staged candidates: precompressed copies, MIME types,
//! conditional requests, cache policy and hidden-file protection.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Request, Response, StatusCode, Uri, header};
use tower::ServiceExt;
use tower_http::services::ServeDir;

#[derive(Clone, PartialEq, Eq)]
pub enum CachePolicy {
    /// Versioned release URLs: content never changes.
    Immutable,
    /// The app's stable URL, served from the active release (named here): every response
    /// revalidates, and validators carry the release so a file from another release never
    /// matches, even with the same size and modification time.
    Current(String),
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

/// The file or directory a request path names under `root`, if the path may be served.
pub fn release_path(root: &Path, rest: &str) -> Option<PathBuf> {
    let path = rest.split('?').next().unwrap_or(rest);
    if !path_allowed(path) {
        return None;
    }
    let decoded = percent_decode(path)?;
    Some(root.join(decoded.trim_start_matches('/')))
}

/// Whether a path missing from the active release may come from an older retained one: plain
/// assets (scripts, WebAssembly, styles, images) that a page opened before an update asks for.
/// Never pages, directories, service workers or manifests, so an old entry point or worker
/// cannot outlive the release that dropped it.
pub fn fallback_allowed(rest: &str) -> bool {
    let path = rest.split('?').next().unwrap_or(rest);
    !revalidated(path)
        && !path.ends_with(".html")
        && path.rsplit('/').next().is_some_and(|f| f.contains('.'))
}

/// Keep only entity tags issued for `release`, without the release prefix, so ServeDir can
/// compare them; tags from other releases never match. Date validators are dropped: equal
/// modification times in two releases do not mean equal files.
fn scope_validators(headers: &mut HeaderMap, release: &str) {
    headers.remove(header::IF_MODIFIED_SINCE);
    headers.remove(header::IF_UNMODIFIED_SINCE);
    let prefix = format!("{release}:");
    // A range continues a download only from the same release; otherwise send the whole file.
    if let Some(if_range) = headers.remove(header::IF_RANGE) {
        let same = if_range
            .to_str()
            .ok()
            .and_then(|t| t.strip_prefix('"')?.strip_suffix('"'))
            .and_then(|inner| inner.strip_prefix(&prefix))
            .and_then(|orig| HeaderValue::from_str(&format!("\"{orig}\"")).ok());
        match same {
            Some(v) => {
                headers.insert(header::IF_RANGE, v);
            }
            None => {
                headers.remove(header::RANGE);
            }
        }
    }
    let Some(inm) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
    else {
        return;
    };
    headers.remove(header::IF_NONE_MATCH);
    let kept: Vec<String> = inm
        .split(',')
        .map(str::trim)
        .filter_map(|tag| {
            let (weak, quoted) = match tag.strip_prefix("W/") {
                Some(rest) => ("W/", rest),
                None => ("", tag),
            };
            let inner = quoted.strip_prefix('"')?.strip_suffix('"')?;
            inner
                .strip_prefix(prefix.as_str())
                .map(|orig| format!("{weak}\"{orig}\""))
        })
        .collect();
    if let Ok(v) = HeaderValue::from_str(&kept.join(", "))
        && !kept.is_empty()
    {
        headers.insert(header::IF_NONE_MATCH, v);
    }
}

/// Prefix the response's entity tag with the release.
fn scope_etag(headers: &mut HeaderMap, release: &str) {
    let Some(tag) = headers.get(header::ETAG).and_then(|v| v.to_str().ok()) else {
        return;
    };
    let (weak, quoted) = match tag.strip_prefix("W/") {
        Some(rest) => ("W/", rest),
        None => ("", tag),
    };
    let inner = quoted.trim_matches('"');
    if let Ok(v) = HeaderValue::from_str(&format!("{weak}\"{release}:{inner}\"")) {
        headers.insert(header::ETAG, v);
    }
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
    if let CachePolicy::Current(release) = &policy {
        scope_validators(&mut parts.headers, release);
    }
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
    if let CachePolicy::Current(release) = &policy {
        scope_etag(res.headers_mut(), release);
    }
    let cache = match (
        &policy,
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
    fn validators_are_scoped_to_the_release() {
        let mut h = HeaderMap::new();
        h.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_static("\"1.0.0:abc\", W/\"0.9.0:abc\", \"x\""),
        );
        h.insert(
            header::IF_MODIFIED_SINCE,
            HeaderValue::from_static("Thu, 01 Jan 1981 00:00:00 GMT"),
        );
        scope_validators(&mut h, "1.0.0");
        assert_eq!(h.get(header::IF_NONE_MATCH).unwrap(), "\"abc\"");
        assert!(h.get(header::IF_MODIFIED_SINCE).is_none());
        let mut other = HeaderMap::new();
        other.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_static("\"1.0.0:abc\""),
        );
        scope_validators(&mut other, "1.1.0");
        assert!(
            other.get(header::IF_NONE_MATCH).is_none(),
            "another release's tag never matches"
        );
        // A range continues only within the same release; otherwise the whole file is sent.
        let ranged = |if_range: &'static str, release: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::RANGE, HeaderValue::from_static("bytes=10-"));
            h.insert(header::IF_RANGE, HeaderValue::from_static(if_range));
            scope_validators(&mut h, release);
            (
                h.get(header::RANGE).is_some(),
                h.get(header::IF_RANGE)
                    .map(|v| v.to_str().unwrap().to_string()),
            )
        };
        assert_eq!(
            ranged("\"1.0.0:abc\"", "1.0.0"),
            (true, Some("\"abc\"".into()))
        );
        assert_eq!(ranged("\"1.0.0:abc\"", "1.1.0"), (false, None));
        assert_eq!(
            ranged("Thu, 01 Jan 1981 00:00:00 GMT", "1.1.0"),
            (false, None)
        );
        let mut res = HeaderMap::new();
        res.insert(header::ETAG, HeaderValue::from_static("W/\"abc\""));
        scope_etag(&mut res, "1.0.0");
        assert_eq!(res.get(header::ETAG).unwrap(), "W/\"1.0.0:abc\"");
    }

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

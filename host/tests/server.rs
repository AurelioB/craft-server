//! HTTP behaviour of the combined server: app routing and caching, hidden paths, compression,
//! activity tracking, sign-in with roles for the apps and /admin in every method, CSRF and host
//! isolation.

mod common;

use std::io::Read;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use common::*;
use craft_host::activity::Activity;
use craft_host::auth::hash_password;
use craft_host::ops::Updater;
use craft_host::server::{Shared, router};

const REPO: &str = "storytold/testcraft";

struct Server {
    url: String,
    shared: Arc<Shared>,
}

fn spawn(app: axum::Router) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let svc = app.into_make_service_with_connect_info::<SocketAddr>();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let l = tokio::net::TcpListener::from_std(listener).unwrap();
                axum::serve(l, svc).await.unwrap();
            })
    });
    url
}

fn start(e: &Env) -> Server {
    let shared = Shared::new(Arc::new(e.cfg.clone()), Arc::new(Activity::new()));
    Server {
        url: spawn(router(shared.clone())),
        shared,
    }
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    fn cookies(&self) -> Vec<String> {
        self.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
            .map(|(_, v)| v.split(';').next().unwrap().to_string())
            .collect()
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .new_agent()
}

fn request(method: &str, url: &str, headers: &[(&str, &str)], body: Option<(&str, &[u8])>) -> Resp {
    let a = agent();
    let result = match (method, body) {
        ("GET", _) => {
            let mut r = a.get(url);
            for (k, v) in headers {
                r = r.header(*k, *v);
            }
            r.call()
        }
        ("HEAD", _) => {
            let mut r = a.head(url);
            for (k, v) in headers {
                r = r.header(*k, *v);
            }
            r.call()
        }
        (_, body) => {
            let mut r = a.post(url);
            for (k, v) in headers {
                r = r.header(*k, *v);
            }
            match body {
                Some((ct, b)) => r.header("Content-Type", ct).send(b),
                None => r.send_empty(),
            }
        }
    };
    let mut resp = result.expect("request");
    let headers = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let mut body = Vec::new();
    let _ = resp.body_mut().as_reader().read_to_end(&mut body);
    Resp {
        status: resp.status().as_u16(),
        headers,
        body,
    }
}

fn get(url: &str, headers: &[(&str, &str)]) -> Resp {
    request("GET", url, headers, None)
}

fn installed_env(gh: &FakeGitHub, sections: &str) -> Env {
    let e = env_with(gh, "", "min_free_space = 0", sections);
    let big = format!("export const data = \"{}\";\n", "craft ".repeat(500));
    gh.publish(
        REPO,
        "v1.0.0",
        release_zip(
            &e.root(),
            "1.0.0",
            &[
                ("big.js", big.as_bytes()),
                ("sw.js", b"self.x=1"),
                ("app.webmanifest", br#"{"name":"TestCraft"}"#),
            ],
        ),
        PublishOpts::default(),
    );
    let u = Updater::new(&e.cfg);
    u.prepare().unwrap();
    assert!(matches!(
        u.update_app(e.cfg.app("testcraft").unwrap(), true),
        craft_host::ops::Outcome::Installed(_)
    ));
    e
}

#[test]
fn apps_are_served_under_stable_and_versioned_paths() {
    let gh = FakeGitHub::start();
    let e = installed_env(&gh, "");
    let s = start(&e);
    let u = &s.url;

    assert_eq!(get(&format!("{u}/healthz"), &[]).status, 200);
    let launcher = get(&format!("{u}/"), &[]);
    assert_eq!(launcher.status, 200);
    assert!(launcher.text().contains("launcher/launcher.js"));
    let status: serde_json::Value =
        serde_json::from_slice(&get(&format!("{u}/status.json"), &[]).body).unwrap();
    assert_eq!(status["apps"][0]["active"], "1.0.0");
    // Custom apps have no built-in logo and this release declares no icon.
    assert!(status["apps"][0]["icon"].is_null());
    for (path, ct) in [
        ("icons/photocraft.webp", "image/webp"),
        ("icons/artcraft.svg", "image/svg+xml"),
        ("fonts/archivo-latin.woff2", "font/woff2"),
    ] {
        let r = get(&format!("{u}/launcher/{path}"), &[]);
        assert_eq!(
            (r.status, r.header("content-type")),
            (200, Some(ct)),
            "{path}"
        );
        assert!(!r.body.is_empty());
    }
    for path in [
        "icons/unknown.webp",
        "icons/../launcher.js",
        "../status.json",
    ] {
        assert_eq!(
            get(&format!("{u}/launcher/{path}"), &[]).status,
            404,
            "{path}"
        );
    }

    let r = get(&format!("{u}/testcraft"), &[]);
    assert_eq!((r.status, r.header("location")), (301, Some("testcraft/")));
    let r = get(&format!("{u}/testcraft/?webgl"), &[]);
    assert_eq!(
        (r.status, r.header("location"), r.header("cache-control")),
        (302, Some("1.0.0/?webgl"), Some("no-cache"))
    );
    let r = get(&format!("{u}/testcraft/1.0.0"), &[]);
    assert_eq!((r.status, r.header("location")), (301, Some("1.0.0/")));

    let index = get(&format!("{u}/testcraft/1.0.0/"), &[]);
    assert_eq!(index.status, 200);
    assert!(
        index
            .header("content-type")
            .unwrap()
            .starts_with("text/html")
    );
    assert_eq!(index.header("cache-control"), Some("no-cache"));
    assert_eq!(index.header("x-content-type-options"), Some("nosniff"));
    let wasm = get(&format!("{u}/testcraft/1.0.0/app_bg.wasm"), &[]);
    assert_eq!(wasm.header("content-type"), Some("application/wasm"));
    assert_eq!(
        wasm.header("cache-control"),
        Some("public, max-age=31536000, immutable")
    );
    let js = get(
        &format!("{u}/testcraft/1.0.0/big.js"),
        &[("Accept-Encoding", "br, gzip")],
    );
    assert_eq!(
        (js.header("content-type"), js.header("content-encoding")),
        (Some("text/javascript"), Some("br"))
    );
    let js = get(
        &format!("{u}/testcraft/1.0.0/big.js"),
        &[("Accept-Encoding", "gzip")],
    );
    assert_eq!(js.header("content-encoding"), Some("gzip"));
    let mut plain = Vec::new();
    flate2::read::GzDecoder::new(&js.body[..])
        .read_to_end(&mut plain)
        .unwrap();
    assert!(
        String::from_utf8(plain)
            .unwrap()
            .starts_with("export const data")
    );
    assert_eq!(
        get(&format!("{u}/testcraft/1.0.0/sw.js"), &[]).header("cache-control"),
        Some("no-cache")
    );
    let etag = get(&format!("{u}/testcraft/1.0.0/app.js"), &[])
        .header("etag")
        .unwrap()
        .to_string();
    assert_eq!(
        get(
            &format!("{u}/testcraft/1.0.0/app.js"),
            &[("If-None-Match", &etag)]
        )
        .status,
        304
    );

    for hidden in [
        "/testcraft/1.0.0/.craft-release.json",
        "/testcraft/1.0.0/%2ecraft-release.json",
        "/testcraft/1.0.0/%2e%2e/1.0.0/index.html",
        "/testcraft/1.0.0/..%2f..%2f..%2fstate%20dir/apps/testcraft.json",
        "/testcraft/current/index.html",
        "/testcraft/9.9.9/index.html",
        "/testcraft/1.0.0/missing.js",
        "/nope/",
        "/.staging/",
    ] {
        assert_eq!(get(&format!("{u}{hidden}"), &[]).status, 404, "{hidden}");
    }
    assert_eq!(
        request("POST", &format!("{u}/testcraft/1.0.0/"), &[], None).status,
        405
    );
    assert_eq!(
        get(&format!("{u}/admin/"), &[]).status,
        404,
        "admin is disabled by default"
    );
    assert!(
        s.shared
            .activity
            .release_used_within("testcraft", "1.0.0", 60)
    );
}

#[test]
fn uninstalled_apps_show_the_installing_page() {
    let gh = FakeGitHub::start();
    let e = env(&gh);
    Updater::new(&e.cfg).prepare().unwrap();
    let s = start(&e);
    let r = get(&format!("{}/testcraft/", s.url), &[]);
    assert_eq!(r.status, 503);
    assert!(r.text().contains("being installed"));
    assert_eq!(get(&format!("{}/readyz/testcraft", s.url), &[]).status, 503);
}

fn csrf_from(r: &Resp) -> String {
    r.cookies()
        .into_iter()
        .find_map(|c| c.strip_prefix("craft_csrf=").map(str::to_string))
        .expect("csrf cookie")
}

fn wait_idle(url: &str, headers: &[(&str, &str)]) -> serde_json::Value {
    for _ in 0..100 {
        let v: serde_json::Value =
            serde_json::from_slice(&get(&format!("{url}/admin/api/status"), headers).body).unwrap();
        if v["running"].is_null() {
            return v;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("job did not finish");
}

#[test]
fn unauthenticated_mode_still_requires_csrf_for_actions() {
    let gh = FakeGitHub::start();
    let e = installed_env(&gh, "[admin]\nenabled = true\n");
    let s = start(&e);
    let u = &s.url;
    assert_eq!(
        get(&format!("{u}/admin"), &[]).header("location"),
        Some("admin/")
    );
    let page = get(&format!("{u}/admin/"), &[]);
    assert_eq!(page.status, 200);
    assert_eq!(page.header("x-frame-options"), Some("DENY"));
    let csrf = csrf_from(&page);
    let cookie = format!("craft_csrf={csrf}");
    let action = format!("{u}/admin/api/apps/testcraft/check");
    assert_eq!(
        request("POST", &action, &[("Cookie", &cookie)], None).status,
        403,
        "no CSRF header"
    );
    assert_eq!(
        request(
            "POST",
            &action,
            &[
                ("Cookie", &cookie),
                ("X-Craft-CSRF", "x".repeat(64).as_str())
            ],
            None
        )
        .status,
        403
    );
    let cross = [
        ("Cookie", cookie.as_str()),
        ("X-Craft-CSRF", csrf.as_str()),
        ("Origin", "https://evil.example"),
    ];
    assert_eq!(
        request("POST", &action, &cross, None).status,
        403,
        "cross-origin request"
    );
    let ok = [("Cookie", cookie.as_str()), ("X-Craft-CSRF", csrf.as_str())];
    assert_eq!(request("POST", &action, &ok, None).status, 202);
    let st = wait_idle(u, &[]);
    assert_eq!(st["results"][0]["action"], "check");
    assert_eq!(st["results"][0]["ok"], true, "{st}");
    assert_eq!(
        request(
            "POST",
            &format!("{u}/admin/api/apps/testcraft/explode"),
            &ok,
            None
        )
        .status,
        404
    );
    assert_eq!(
        request("POST", &format!("{u}/admin/api/apps/nope/check"), &ok, None).status,
        404
    );
    assert_eq!(
        request(
            "POST",
            &format!("{u}/admin/api/apps/testcraft/pin"),
            &ok,
            Some(("application/json", b"{}"))
        )
        .status,
        400
    );
}

fn write_users(e: &Env, lines: &[(&str, &str, &str)]) {
    let text: String = lines
        .iter()
        .map(|(name, pw, role)| format!("{name}:{}:{role}\n", hash_password(pw).unwrap()))
        .collect();
    std::fs::write(e.cfg.auth.users_file.clone(), text).unwrap();
}

#[test]
fn basic_auth_protects_admin_by_role_and_leaves_public_apps_open() {
    let gh = FakeGitHub::start();
    let e = installed_env(&gh, "[auth]\nmethod = \"basic\"\n[admin]\nenabled = true\n");
    write_users(&e, &[("ana", "s3cret", "admin"), ("bo", "bo-pw", "user")]);
    let s = start(&e);
    let u = &s.url;
    let r = get(&format!("{u}/admin/"), &[]);
    assert_eq!(r.status, 401);
    assert!(r.header("www-authenticate").unwrap().starts_with("Basic"));
    let wrong = format!("Basic {}", STANDARD.encode("ana:nope"));
    assert_eq!(
        get(
            &format!("{u}/admin/api/status"),
            &[("Authorization", &wrong)]
        )
        .status,
        401
    );
    let good = format!("Basic {}", STANDARD.encode("ana:s3cret"));
    let st = get(
        &format!("{u}/admin/api/status"),
        &[("Authorization", &good)],
    );
    assert_eq!(st.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&st.body).unwrap();
    assert_eq!(
        (v["user"].as_str(), v["auth"].as_str()),
        (Some("ana"), Some("basic"))
    );
    let user = format!("Basic {}", STANDARD.encode("bo:bo-pw"));
    assert_eq!(
        get(
            &format!("{u}/admin/api/status"),
            &[("Authorization", &user)]
        )
        .status,
        403,
        "the user role may not manage updates"
    );
    assert_eq!(
        request(
            "POST",
            &format!("{u}/admin/api/apps/testcraft/check"),
            &[],
            None
        )
        .status,
        401
    );
    // [auth] apps defaults to public.
    assert_eq!(get(&format!("{u}/testcraft/"), &[]).status, 302);
    assert_eq!(get(&format!("{u}/status.json"), &[]).status, 200);
}

fn session_from(r: &Resp) -> String {
    r.cookies()
        .into_iter()
        .find(|c| c.starts_with("craft_session="))
        .expect("session cookie")
}

fn form_login(u: &str, user: &str, pw: &str, next: &str) -> Resp {
    let body = format!("user={user}&password={pw}&next={next}");
    request(
        "POST",
        &format!("{u}/auth/login"),
        &[],
        Some(("application/x-www-form-urlencoded", body.as_bytes())),
    )
}

#[test]
fn form_login_sessions_and_logout() {
    let gh = FakeGitHub::start();
    let e = installed_env(&gh, "[auth]\nmethod = \"form\"\n[admin]\nenabled = true\n");
    write_users(&e, &[("ana", "s3cret", "admin")]);
    let s = start(&e);
    let u = &s.url;
    assert_eq!(
        get(&format!("{u}/admin/"), &[]).header("location"),
        Some("../auth/login?next=admin%2F")
    );
    assert_eq!(get(&format!("{u}/admin/api/status"), &[]).status, 401);
    let login_page = get(&format!("{u}/auth/login"), &[]);
    assert!(login_page.text().contains("Sign in"));
    assert_eq!(get(&format!("{u}/auth/style.css"), &[]).status, 200);

    let bad = form_login(u, "ana", "x", "admin%2F");
    assert_eq!(
        (bad.status, bad.header("location")),
        (303, Some("login?failed=1&next=admin%2F"))
    );
    assert!(bad.cookies().is_empty());
    let cross = request(
        "POST",
        &format!("{u}/auth/login"),
        &[("Origin", "https://evil.example")],
        Some((
            "application/x-www-form-urlencoded",
            b"user=ana&password=s3cret".as_slice(),
        )),
    );
    assert_eq!(cross.status, 403, "login CSRF");
    let offsite = form_login(u, "ana", "s3cret", "%2F%2Fevil.example%2F");
    assert_eq!(
        offsite.header("location"),
        Some("../"),
        "off-site targets fall back to the launcher"
    );

    let ok = form_login(u, "ana", "s3cret", "admin%2F");
    assert_eq!((ok.status, ok.header("location")), (303, Some("../admin/")));
    let set = ok
        .headers
        .iter()
        .find(|(k, _)| k == "set-cookie")
        .unwrap()
        .1
        .clone();
    assert!(
        set.contains("HttpOnly") && set.contains("SameSite=Lax") && set.contains("Path=/;"),
        "{set}"
    );
    let session = session_from(&ok);
    let page = get(&format!("{u}/admin/"), &[("Cookie", &session)]);
    assert_eq!(page.status, 200);
    let csrf = csrf_from(&page);
    let cookies = format!("{session}; craft_csrf={csrf}");
    let v: serde_json::Value = serde_json::from_slice(
        &get(&format!("{u}/admin/api/status"), &[("Cookie", &cookies)]).body,
    )
    .unwrap();
    assert_eq!(v["user"], "ana");
    assert_eq!(v["can_logout"], true);

    let cross_out = request(
        "POST",
        &format!("{u}/auth/logout"),
        &[("Cookie", &cookies), ("Origin", "https://evil.example")],
        None,
    );
    assert_eq!(cross_out.status, 403, "cross-site logout");
    let out = request(
        "POST",
        &format!("{u}/auth/logout"),
        &[("Cookie", &cookies)],
        None,
    );
    assert_eq!(out.status, 200);
    assert_eq!(
        get(&format!("{u}/admin/api/status"), &[("Cookie", &cookies)]).status,
        401,
        "session revoked"
    );
}

#[test]
fn signed_in_apps_need_a_role_and_admin_needs_the_admin_role() {
    let gh = FakeGitHub::start();
    let e = installed_env(
        &gh,
        "[auth]\nmethod = \"form\"\napps = \"signed-in\"\n[admin]\nenabled = true\n",
    );
    write_users(&e, &[("ana", "s3cret", "admin"), ("bo", "bo-pw", "user")]);
    let s = start(&e);
    let u = &s.url;
    let html = [("Accept", "text/html")];
    let nav = [("Sec-Fetch-Mode", "navigate")];

    // Page loads go to the sign-in page and come back; other requests get 401.
    assert_eq!(
        get(&format!("{u}/"), &html).header("location"),
        Some("auth/login?next=")
    );
    let r = get(&format!("{u}/testcraft/1.0.0/index.html?x=1"), &nav);
    assert_eq!(
        (r.status, r.header("location")),
        (
            303,
            Some("../../auth/login?next=testcraft%2F1.0.0%2Findex.html%3Fx%3D1")
        )
    );
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert_eq!(get(&format!("{u}/testcraft/1.0.0/big.js"), &[]).status, 401);
    assert_eq!(get(&format!("{u}/status.json"), &[]).status, 401);
    // Health, sign-in, the launcher's logos and fonts, and web app manifests (fetched without
    // cookies) stay reachable; a missing manifest is 404, not 401.
    for open in ["/healthz", "/auth/login", "/launcher/icons/photocraft.webp"] {
        assert_eq!(get(&format!("{u}{open}"), &[]).status, 200, "{open}");
    }
    let manifest = get(&format!("{u}/testcraft/1.0.0/app.webmanifest"), &[]);
    assert_eq!(manifest.status, 200);
    assert!(manifest.text().contains("TestCraft"));
    // Only real manifests: encoded lookalikes do not open other release files.
    for lookalike in [
        "big.js%3F.webmanifest",
        "big.js%23.webmanifest",
        "big.js%2F.webmanifest",
    ] {
        let r = get(&format!("{u}/testcraft/1.0.0/{lookalike}"), &[]);
        assert_eq!(r.status, 404, "{lookalike}");
        assert!(!r.text().contains("craft craft"), "{lookalike}");
    }
    assert_eq!(get(&format!("{u}/auth/me"), &[]).status, 401);

    let user = session_from(&form_login(u, "bo", "bo-pw", "testcraft%2F"));
    let as_user = [("Cookie", user.as_str())];
    assert_eq!(get(&format!("{u}/testcraft/"), &as_user).status, 302);
    assert_eq!(
        get(&format!("{u}/testcraft/1.0.0/big.js"), &as_user).status,
        200
    );
    assert_eq!(get(&format!("{u}/status.json"), &as_user).status, 200);
    assert_eq!(get(&format!("{u}/"), &as_user).status, 200);
    assert_eq!(get(&format!("{u}/admin/"), &as_user).status, 403);
    assert_eq!(get(&format!("{u}/admin/api/status"), &as_user).status, 403);
    let me: serde_json::Value =
        serde_json::from_slice(&get(&format!("{u}/auth/me"), &as_user).body).unwrap();
    assert_eq!(
        (
            me["user"].as_str(),
            me["role"].as_str(),
            me["admin_url"].is_null()
        ),
        (Some("bo"), Some("user"), true)
    );

    let admin = session_from(&form_login(u, "ana", "s3cret", ""));
    let as_admin = [("Cookie", admin.as_str())];
    assert_eq!(get(&format!("{u}/testcraft/"), &as_admin).status, 302);
    assert_eq!(get(&format!("{u}/admin/"), &as_admin).status, 200);
    let me: serde_json::Value =
        serde_json::from_slice(&get(&format!("{u}/auth/me"), &as_admin).body).unwrap();
    assert_eq!(
        (
            me["role"].as_str(),
            me["admin_url"].as_str(),
            me["can_logout"].as_bool()
        ),
        (Some("admin"), Some("admin/"), Some(true))
    );

    request("POST", &format!("{u}/auth/logout"), &as_user, None);
    assert_eq!(
        get(
            &format!("{u}/testcraft/"),
            &[("Cookie", user.as_str()), ("Accept", "text/html")]
        )
        .status,
        303,
        "signed out"
    );
}

#[test]
fn proxy_identities_only_from_trusted_peers_with_a_role() {
    let gh = FakeGitHub::start();
    let trusted = installed_env(
        &gh,
        "[server]\ntrusted_proxies = [\"127.0.0.0/8\"]\n[auth]\nmethod = \"proxy\"\nadmin_groups = [\"craft-admins\"]\nuser_groups = [\"craft-users\"]\napps = \"signed-in\"\n[admin]\nenabled = true\n",
    );
    let s = start(&trusted);
    let u = &s.url;
    assert_eq!(get(&format!("{u}/admin/"), &[]).status, 403, "no identity");
    let member = [
        ("Remote-User", "ana"),
        ("Remote-Groups", "users|craft-admins"),
    ];
    assert_eq!(get(&format!("{u}/admin/"), &member).status, 200);
    let user = [("Remote-User", "bob"), ("Remote-Groups", "craft-users")];
    assert_eq!(get(&format!("{u}/admin/"), &user).status, 403);
    assert_eq!(get(&format!("{u}/testcraft/"), &user).status, 302);
    let outsider = [("Remote-User", "cy"), ("Remote-Groups", "staff")];
    assert_eq!(get(&format!("{u}/testcraft/"), &outsider).status, 403);

    let untrusted = installed_env(
        &gh,
        "[server]\ntrusted_proxies = [\"10.0.0.0/8\"]\n[auth]\nmethod = \"proxy\"\nadmin_groups = [\"craft-admins\"]\n[admin]\nenabled = true\n",
    );
    let s = start(&untrusted);
    assert_eq!(
        get(&format!("{}/admin/", s.url), &member).status,
        403,
        "forged header from an untrusted peer"
    );
}

#[test]
fn admin_host_separates_the_admin_origin_from_the_apps() {
    let gh = FakeGitHub::start();
    let e = installed_env(
        &gh,
        "[admin]\nenabled = true\nhost = \"admin.example.net\"\n",
    );
    let s = start(&e);
    let u = &s.url;
    assert_eq!(
        get(&format!("{u}/admin/"), &[]).status,
        404,
        "not on the apps' host"
    );
    assert_eq!(
        get(&format!("{u}/admin/"), &[("Host", "admin.example.net")]).status,
        200
    );
    assert_eq!(
        get(
            &format!("{u}/admin/"),
            &[("Host", "admin.example.net:8080")]
        )
        .status,
        200,
        "port ignored"
    );
    assert_eq!(
        get(
            &format!("{u}/testcraft/"),
            &[("Host", "admin.example.net:8080")]
        )
        .status,
        404
    );
    assert_eq!(
        get(
            &format!("{u}/admin/"),
            &[("Host", "admin.example.net.evil:8080")]
        )
        .status,
        404
    );
    assert_eq!(
        get(&format!("{u}/testcraft/"), &[("Host", "admin.example.net")]).status,
        404,
        "apps are not served on the admin host"
    );
    assert_eq!(
        get(&format!("{u}/"), &[("Host", "admin.example.net")]).status,
        404
    );
    // No app or launcher code runs on the admin host, so no upstream script shares its origin.
    for path in [
        "/testcraft/1.0.0/index.html",
        "/testcraft/1.0.0/big.js",
        "/launcher/launcher.js",
        "/status.json",
    ] {
        assert_eq!(
            get(&format!("{u}{path}"), &[("Host", "admin.example.net")]).status,
            404,
            "{path} on the admin host"
        );
    }
    // An admin action sent from a page on the apps' host is refused, even with the CSRF pair.
    let page = get(&format!("{u}/admin/"), &[("Host", "admin.example.net")]);
    let csrf = csrf_from(&page);
    let cookie = format!("craft_csrf={csrf}");
    let from_apps = request(
        "POST",
        &format!("{u}/admin/api/apps/testcraft/check"),
        &[
            ("Host", "admin.example.net"),
            ("Cookie", &cookie),
            ("X-Craft-CSRF", &csrf),
            ("Origin", "http://apps.example.net"),
        ],
        None,
    );
    assert_eq!(from_apps.status, 403);
    assert!(
        !page
            .headers
            .iter()
            .any(|(k, v)| k == "set-cookie" && v.to_ascii_lowercase().contains("domain=")),
        "cookies are host-only"
    );
    assert_eq!(get(&format!("{u}/testcraft/"), &[]).status, 302);
    for host in ["admin.example.net", "apps.example.net"] {
        assert_eq!(get(&format!("{u}/healthz"), &[("Host", host)]).status, 200);
        assert_eq!(
            get(&format!("{u}/auth/me"), &[("Host", host)]).status,
            200,
            "sign-in answers on every host"
        );
    }

    // Behind a trusted proxy the forwarded host decides; from other peers it is ignored.
    let behind = installed_env(
        &gh,
        "[server]\ntrusted_proxies = [\"127.0.0.0/8\"]\n[admin]\nenabled = true\nhost = \"admin.example.net\"\n",
    );
    let s = start(&behind);
    let fwd_admin = [("X-Forwarded-Host", "admin.example.net")];
    assert_eq!(get(&format!("{}/admin/", s.url), &fwd_admin).status, 200);
    assert_eq!(
        get(&format!("{}/testcraft/", s.url), &fwd_admin).status,
        404
    );
    assert_eq!(
        get(
            &format!("{}/admin/", s.url),
            &[("X-Forwarded-Host", "apps.example.net")]
        )
        .status,
        404
    );
    let direct = installed_env(
        &gh,
        "[server]\ntrusted_proxies = [\"10.0.0.0/8\"]\n[admin]\nenabled = true\nhost = \"admin.example.net\"\n",
    );
    let s = start(&direct);
    assert_eq!(
        get(&format!("{}/admin/", s.url), &fwd_admin).status,
        404,
        "forged forwarded host"
    );
    assert_eq!(
        get(&format!("{}/testcraft/", s.url), &fwd_admin).status,
        302
    );
}

fn id_token(claims: serde_json::Value) -> String {
    format!(
        "eyJhbGciOiJSUzI1NiJ9.{}.c2ln",
        URL_SAFE_NO_PAD.encode(claims.to_string())
    )
}

fn param<'a>(url: &'a str, name: &str) -> &'a str {
    url.split(['?', '&'])
        .find_map(|kv| kv.strip_prefix(&format!("{name}=")))
        .unwrap()
}

#[test]
fn oidc_login_validates_state_nonce_and_maps_groups_to_roles() {
    let gh = FakeGitHub::start();
    let issuer = format!("{}/oidc", gh.url);
    let sections = format!(
        "[auth]\nmethod = \"oidc\"\napps = \"signed-in\"\nadmin_groups = [\"craft-admins\"]\nuser_groups = [\"craft-users\"]\n[auth.oidc]\nissuer = \"{issuer}\"\nclient_id = \"craft\"\nredirect_url = \"https://apps.example.net/auth/oidc/callback\"\n[admin]\nenabled = true\n"
    );
    let e = installed_env(&gh, &sections);
    gh.put_route(
        "/oidc/.well-known/openid-configuration",
        serde_json::json!({"issuer": issuer, "authorization_endpoint": format!("{issuer}/authorize"), "token_endpoint": format!("{issuer}/token")})
            .to_string()
            .into_bytes(),
    );
    let s = start(&e);
    let u = &s.url;
    assert_eq!(
        get(&format!("{u}/admin/"), &[]).header("location"),
        Some("../auth/oidc/login?next=admin%2F")
    );
    assert_eq!(
        get(&format!("{u}/testcraft/"), &[("Accept", "text/html")]).header("location"),
        Some("../auth/oidc/login?next=testcraft%2F")
    );

    let login = |next: &str, claims: &dyn Fn(&str) -> serde_json::Value| -> Resp {
        let start = get(&format!("{u}/auth/oidc/login?next={next}"), &[]);
        assert_eq!(start.status, 303);
        let loc = start.header("location").unwrap().to_string();
        assert!(
            loc.starts_with(&format!("{issuer}/authorize?"))
                && loc.contains("code_challenge_method=S256"),
            "{loc}"
        );
        let (state, nonce) = (
            param(&loc, "state").to_string(),
            param(&loc, "nonce").to_string(),
        );
        gh.put_route(
            "/oidc/token",
            serde_json::json!({"id_token": id_token(claims(&nonce)), "token_type": "Bearer"})
                .to_string()
                .into_bytes(),
        );
        let callback = format!("{u}/auth/oidc/callback?code=abc&state={state}");
        let bound = start
            .cookies()
            .into_iter()
            .find(|c| c.starts_with("craft_oidc_state="))
            .expect("state cookie");
        assert!(bound.ends_with(&state), "{bound}");
        // The same callback in another browser (no state cookie) is refused: no login CSRF.
        let elsewhere = get(&callback, &[]);
        assert_eq!(elsewhere.status, 403);
        assert!(elsewhere.cookies().is_empty());
        get(&callback, &[("Cookie", bound.as_str())])
    };
    let exp = craft_host::timeutil::now_epoch() + 300;
    let iss = issuer.as_str();
    let with_groups = |groups: serde_json::Value| move |nonce: &str| serde_json::json!({"iss": iss, "aud": "craft", "exp": exp, "nonce": nonce, "preferred_username": "ana", "groups": groups.clone()});

    let ok = login(
        "admin%2F",
        &with_groups(serde_json::json!(["craft-admins"])),
    );
    assert_eq!(
        (ok.status, ok.header("location")),
        (303, Some("../../admin/"))
    );
    let session = session_from(&ok);
    let v: serde_json::Value = serde_json::from_slice(
        &get(&format!("{u}/admin/api/status"), &[("Cookie", &session)]).body,
    )
    .unwrap();
    assert_eq!(
        (v["user"].as_str(), v["auth"].as_str()),
        (Some("ana"), Some("oidc"))
    );

    let user = login(
        "testcraft%2F",
        &with_groups(serde_json::json!(["craft-users"])),
    );
    assert_eq!(user.header("location"), Some("../../testcraft/"));
    let user = session_from(&user);
    assert_eq!(
        get(&format!("{u}/testcraft/"), &[("Cookie", &user)]).status,
        302
    );
    assert_eq!(
        get(&format!("{u}/admin/api/status"), &[("Cookie", &user)]).status,
        403
    );

    let good = with_groups(serde_json::json!(["craft-admins"]));
    let wrong_nonce = login("", &|_n: &str| good("not-the-nonce"));
    assert_eq!(wrong_nonce.status, 403);
    let no_role = login("", &with_groups(serde_json::json!(["staff"])));
    assert_eq!(no_role.status, 403);
    assert!(no_role.cookies().is_empty());
    assert_eq!(
        get(
            &format!("{u}/auth/oidc/callback?code=abc&state=forged"),
            &[("Cookie", "craft_oidc_state=forged")]
        )
        .status,
        403,
        "state never issued by this server"
    );
}

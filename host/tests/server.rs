//! HTTP behaviour of the combined server: app routing and caching, hidden paths, compression,
//! activity tracking, and every administration auth mode including CSRF and host isolation.

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

fn start(e: &Env) -> Server {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let shared = Shared::new(Arc::new(e.cfg.clone()), Arc::new(Activity::new()));
    let app = router(shared.clone()).into_make_service_with_connect_info::<SocketAddr>();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let l = tokio::net::TcpListener::from_std(listener).unwrap();
                axum::serve(l, app).await.unwrap();
            })
    });
    Server { url, shared }
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
            &[("big.js", big.as_bytes()), ("sw.js", b"self.x=1")],
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
    let e = installed_env(&gh, "[admin]\nauth = \"none\"\nshared_origin = true\n");
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

#[test]
fn basic_auth_protects_pages_and_api() {
    let gh = FakeGitHub::start();
    let e = installed_env(&gh, "[admin]\nauth = \"basic\"\nshared_origin = true\n");
    std::fs::write(
        e.cfg.admin.users_file.clone(),
        format!("ana:{}\n", hash_password("s3cret").unwrap()),
    )
    .unwrap();
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
}

#[test]
fn form_login_sessions_and_logout() {
    let gh = FakeGitHub::start();
    let e = installed_env(&gh, "[admin]\nauth = \"form\"\nshared_origin = true\n");
    std::fs::write(
        e.cfg.admin.users_file.clone(),
        format!("ana:{}\n", hash_password("s3cret").unwrap()),
    )
    .unwrap();
    let s = start(&e);
    let u = &s.url;
    assert_eq!(
        get(&format!("{u}/admin/"), &[]).header("location"),
        Some("login")
    );
    assert_eq!(get(&format!("{u}/admin/api/status"), &[]).status, 401);
    assert!(
        get(&format!("{u}/admin/login"), &[])
            .text()
            .contains("Sign in")
    );

    let form = |pw: &str| format!("user=ana&password={pw}");
    let bad = request(
        "POST",
        &format!("{u}/admin/login"),
        &[],
        Some(("application/x-www-form-urlencoded", form("x").as_bytes())),
    );
    assert_eq!(
        (bad.status, bad.header("location")),
        (303, Some("login?failed=1"))
    );
    assert!(bad.cookies().is_empty());
    let cross = request(
        "POST",
        &format!("{u}/admin/login"),
        &[("Origin", "https://evil.example")],
        Some((
            "application/x-www-form-urlencoded",
            form("s3cret").as_bytes(),
        )),
    );
    assert_eq!(cross.status, 403, "login CSRF");

    let ok = request(
        "POST",
        &format!("{u}/admin/login"),
        &[],
        Some((
            "application/x-www-form-urlencoded",
            form("s3cret").as_bytes(),
        )),
    );
    assert_eq!(ok.status, 303);
    let set = ok
        .headers
        .iter()
        .find(|(k, _)| k == "set-cookie")
        .unwrap()
        .1
        .clone();
    assert!(
        set.contains("HttpOnly") && set.contains("SameSite=Strict") && set.contains("Path=/admin"),
        "{set}"
    );
    let session = ok
        .cookies()
        .into_iter()
        .find(|c| c.starts_with("craft_admin="))
        .unwrap();
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

    let out = request(
        "POST",
        &format!("{u}/admin/logout"),
        &[("Cookie", &cookies), ("X-Craft-CSRF", &csrf)],
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
fn proxy_identities_only_from_trusted_peers_and_allowed_groups() {
    let gh = FakeGitHub::start();
    let trusted = installed_env(
        &gh,
        "[server]\ntrusted_proxies = [\"127.0.0.0/8\"]\n[admin]\nauth = \"proxy\"\nshared_origin = true\nallowed_groups = [\"craft-admins\"]\n",
    );
    let s = start(&trusted);
    let u = &s.url;
    assert_eq!(get(&format!("{u}/admin/"), &[]).status, 403, "no identity");
    let member = [
        ("Remote-User", "ana"),
        ("Remote-Groups", "users|craft-admins"),
    ];
    assert_eq!(get(&format!("{u}/admin/"), &member).status, 200);
    assert_eq!(
        get(
            &format!("{u}/admin/"),
            &[("Remote-User", "bob"), ("Remote-Groups", "users")]
        )
        .status,
        403
    );

    let untrusted = installed_env(
        &gh,
        "[server]\ntrusted_proxies = [\"10.0.0.0/8\"]\n[admin]\nauth = \"proxy\"\nshared_origin = true\n",
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
        "[admin]\nauth = \"none\"\nhost = \"admin.example.net\"\n",
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
    assert_eq!(get(&format!("{u}/testcraft/"), &[]).status, 302);
    assert_eq!(
        get(&format!("{u}/healthz"), &[("Host", "admin.example.net")]).status,
        200
    );

    // Behind a trusted proxy the forwarded host decides; from other peers it is ignored.
    let behind = installed_env(
        &gh,
        "[server]\ntrusted_proxies = [\"127.0.0.0/8\"]\n[admin]\nauth = \"none\"\nhost = \"admin.example.net\"\n",
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
        "[server]\ntrusted_proxies = [\"10.0.0.0/8\"]\n[admin]\nauth = \"none\"\nhost = \"admin.example.net\"\n",
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
fn oidc_login_validates_state_nonce_and_authorization() {
    let gh = FakeGitHub::start();
    let issuer = format!("{}/oidc", gh.url);
    let sections = format!(
        "[admin]\nauth = \"oidc\"\nshared_origin = true\nallowed_groups = [\"craft-admins\"]\n[admin.oidc]\nissuer = \"{issuer}\"\nclient_id = \"craft\"\nredirect_url = \"https://apps.example.net/admin/oidc/callback\"\n"
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
        Some("oidc/login")
    );

    let login = |claims: &dyn Fn(&str) -> serde_json::Value| -> Resp {
        let start = get(&format!("{u}/admin/oidc/login"), &[]);
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
        get(
            &format!("{u}/admin/oidc/callback?code=abc&state={state}"),
            &[],
        )
    };
    let exp = craft_host::timeutil::now_epoch() + 300;
    let good = |nonce: &str| serde_json::json!({"iss": issuer, "aud": "craft", "exp": exp, "nonce": nonce, "preferred_username": "ana", "groups": ["craft-admins"]});

    let ok = login(&good);
    assert_eq!((ok.status, ok.header("location")), (303, Some("../")));
    let session = ok
        .cookies()
        .into_iter()
        .find(|c| c.starts_with("craft_admin="))
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(
        &get(&format!("{u}/admin/api/status"), &[("Cookie", &session)]).body,
    )
    .unwrap();
    assert_eq!(
        (v["user"].as_str(), v["auth"].as_str()),
        (Some("ana"), Some("oidc"))
    );

    let wrong_nonce = login(&|_n: &str| good("not-the-nonce"));
    assert_eq!(wrong_nonce.status, 403);
    let not_admin = login(&|n: &str| {
        let mut c = good(n);
        c["groups"] = serde_json::json!(["users"]);
        c
    });
    assert_eq!(not_admin.status, 403);
    assert!(not_admin.cookies().is_empty());
    assert_eq!(
        get(
            &format!("{u}/admin/oidc/callback?code=abc&state=forged"),
            &[]
        )
        .status,
        403
    );
}

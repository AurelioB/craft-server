//! Docker Compose integration test of the single-image deployment: host paths with spaces,
//! serving, form login and updates from /admin without a restart, concurrent CLI updates, disk
//! exhaustion, offline restarts and running under a different UID with a supplemental group.
//! Uses a static fake GitHub on the project network.
//!
//! Needs Docker with Compose v2: `cargo test --test compose -- --ignored --nocapture`.

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;
use sha2::{Digest, Sha256};

struct Stack {
    root: PathBuf,
    project: String,
    env_file: PathBuf,
    base: PathBuf,
    port: u16,
    gid: u32,
    _tmp: tempfile::TempDir,
}

fn hex_sha(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

struct Resp {
    status: u16,
    location: String,
    cookies: Vec<String>,
    body: String,
}

impl Stack {
    fn command(&self, args: &[&str], envs: &[(&str, &str)]) -> Command {
        let mut c = Command::new("docker");
        c.current_dir(&self.root)
            .args(["compose", "-p", &self.project, "--env-file"])
            .arg(&self.env_file)
            .args([
                "-f",
                "compose.yaml",
                "-f",
                "tests/integration/compose.test.yaml",
            ])
            .args(args)
            .stdin(Stdio::null());
        for (k, v) in envs {
            c.env(k, v);
        }
        c
    }

    fn compose(&self, args: &[&str], envs: &[(&str, &str)]) -> Output {
        let out = self.command(args, envs).output().expect("docker compose");
        eprintln!(
            "$ compose {} -> {}\n{}{}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.compose(args, &[]);
        assert!(out.status.success(), "compose {args:?} failed");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn http(&self, method: &str, path: &str, headers: &[(&str, &str)], form: Option<&str>) -> Resp {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .new_agent();
        let url = format!("http://127.0.0.1:{}{path}", self.port);
        let result = if method == "GET" {
            let mut r = agent.get(&url);
            for (k, v) in headers {
                r = r.header(*k, *v);
            }
            r.call()
        } else {
            let mut r = agent.post(&url);
            for (k, v) in headers {
                r = r.header(*k, *v);
            }
            match form {
                Some(f) => r
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .send(f.as_bytes()),
                None => r.send_empty(),
            }
        };
        let mut resp = result.expect("http");
        let header = |k: &str| {
            resp.headers()
                .get(k)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string()
        };
        let location = header("location");
        let cookies = resp
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap().to_string())
            .collect();
        let mut body = String::new();
        let _ = resp
            .body_mut()
            .as_reader()
            .take(1 << 20)
            .read_to_string(&mut body);
        Resp {
            status: resp.status().as_u16(),
            location,
            cookies,
            body,
        }
    }

    fn get(&self, path: &str) -> Resp {
        self.http("GET", path, &[], None)
    }

    fn wait_for(&self, what: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(&f)).unwrap_or(false) {
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        let _ = self.compose(&["logs", "--tail", "60"], &[]);
        panic!("timed out waiting for {what}");
    }

    fn fakegh(&self) -> PathBuf {
        self.base.join("fake github")
    }

    /// Add a release to the fake GitHub, newest first.
    fn publish(&self, version: &str, extra: &[(&str, Vec<u8>)]) {
        let top = format!("testcraft-web-{version}");
        let mut entries = craft_host::testutil::site_entries(&top);
        entries.push((format!("{top}/version.txt"), version.as_bytes().to_vec()));
        for (n, d) in extra {
            entries.push((format!("{top}/{n}"), d.clone()));
        }
        let name = format!("testcraft-web-{version}.zip");
        let dl = self.fakegh().join("dl").join(version);
        fs::create_dir_all(&dl).unwrap();
        let refs: Vec<(&str, &[u8])> = entries
            .iter()
            .map(|(n, d)| (n.as_str(), d.as_slice()))
            .collect();
        craft_host::testutil::deflated_zip(&dl.join(&name), &refs);
        let data = fs::read(dl.join(&name)).unwrap();
        fs::write(
            dl.join("SHA256SUMS.txt"),
            format!("{}  {name}\n", hex_sha(&data)),
        )
        .unwrap();
        let list_path = self.fakegh().join("repos/storytold/testcraft/releases");
        let mut list: Vec<serde_json::Value> = fs::read(&list_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        list.insert(
            0,
            json!({
                "tag_name": format!("v{version}"), "draft": false, "prerelease": false,
                "published_at": "2026-10-08T00:00:00Z", "html_url": "",
                "assets": [
                    {"name": name, "size": data.len(), "browser_download_url": format!("http://fakegh:8080/dl/{version}/{name}"), "digest": format!("sha256:{}", hex_sha(&data))},
                    {"name": "SHA256SUMS.txt", "size": 1, "browser_download_url": format!("http://fakegh:8080/dl/{version}/SHA256SUMS.txt")}
                ]
            }),
        );
        fs::create_dir_all(list_path.parent().unwrap()).unwrap();
        fs::write(&list_path, serde_json::to_vec(&list).unwrap()).unwrap();
        for e in walk(&self.fakegh()) {
            let mode = if e.is_dir() { 0o755 } else { 0o644 };
            fs::set_permissions(&e, fs::Permissions::from_mode(mode)).unwrap();
        }
    }

    fn started_at(&self) -> String {
        let id = self.ok(&["ps", "-q", "host"]).trim().to_string();
        let out = Command::new("docker")
            .args(["inspect", "-f", "{{.State.StartedAt}}", &id])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn served(&self) -> String {
        self.get("/testcraft/").location
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![dir.to_path_buf()];
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.file_type().unwrap().is_dir() {
            out.extend(walk(&e.path()));
        } else {
            out.push(e.path());
        }
    }
    out
}

impl Drop for Stack {
    fn drop(&mut self) {
        let _ = self.compose(&["down", "--remove-orphans", "--timeout", "2"], &[]);
        let _ = craft_host::fsutil::remove_tree(&self.base);
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn setup() -> Stack {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("craft host dirs");
    // SAFETY: plain identity queries.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    for d in ["config", "data", "state", "cache", "work", "fake github"] {
        fs::create_dir_all(base.join(d)).unwrap();
        // tempdir() creates 0700 trees; the group shares access for the foreign-UID scenario.
        fs::set_permissions(base.join(d), fs::Permissions::from_mode(0o2770)).unwrap();
    }
    fs::set_permissions(base.join("config"), fs::Permissions::from_mode(0o2750)).unwrap();
    fs::set_permissions(base.join("fake github"), fs::Permissions::from_mode(0o755)).unwrap();
    for p in [tmp.path(), base.as_path()] {
        fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut cfg = String::from(
        r#"
[server]
trusted_proxies = []
[updater]
github_api_url = "http://fakegh:8080"
max_retries = 1
retry_backoff = 0
heartbeat_interval = "5s"
[limits]
min_free_space = 0
[auth]
method = "form"
[admin]
enabled = true
[apps.testcraft]
name = "TestCraft"
repository = "storytold/testcraft"
artifact_patterns = ["testcraft-web-{version}.zip"]
"#,
    );
    for id in [
        "photocraft",
        "vectorcraft",
        "filmcraft",
        "lightcraft",
        "pdfcraft",
        "effectcraft",
        "designcraft",
    ] {
        cfg.push_str(&format!("[apps.{id}]\nenabled = false\n"));
    }
    fs::write(base.join("config/config.toml"), cfg).unwrap();
    fs::write(
        base.join("config/users"),
        format!(
            "ana:{}:admin\n",
            craft_host::auth::hash_password("s3cret").unwrap()
        ),
    )
    .unwrap();
    for f in ["config/config.toml", "config/users"] {
        fs::set_permissions(base.join(f), fs::Permissions::from_mode(0o640)).unwrap();
    }
    let port = free_port();
    let env_file = base.join("test.env");
    let d = |n: &str| base.join(n).display().to_string();
    fs::write(
        &env_file,
        format!(
            "RUN_UID={uid}\nRUN_GID={gid}\nFAKEGH_UID={uid}\nFAKEGH_GID={gid}\nFILE_UMASK=0007\nCONFIG_DIR={}\nDATA_DIR={}\nSTATE_DIR={}\nCACHE_DIR={}\nWORK_DIR={}\nFAKEGH_DIR={}\nBIND_ADDRESS=127.0.0.1\nPORT={port}\n",
            d("config"),
            d("data"),
            d("state"),
            d("cache"),
            d("work"),
            d("fake github")
        ),
    )
    .unwrap();
    let stack = Stack {
        root,
        project: format!("craft-it-{}", std::process::id()),
        env_file,
        base,
        port,
        gid,
        _tmp: tmp,
    };
    stack.publish("1.0.0", &[]);
    stack
}

#[test]
#[ignore = "needs Docker; run with: cargo test --test compose -- --ignored"]
fn compose_stack_end_to_end() {
    let s = setup();
    assert!(s.compose(&["build", "host"], &[]).status.success());
    s.ok(&["up", "-d"]);

    // First start: installs (serving check through the built-in candidate listener), then serves.
    s.wait_for("testcraft ready", || {
        s.get("/readyz/testcraft").status == 200
    });
    let r = s.get("/testcraft/?webgl");
    assert_eq!((r.status, r.location.as_str()), (302, "1.0.0/?webgl"));
    assert_eq!(s.get("/testcraft/1.0.0/version.txt").body, "1.0.0");
    assert_eq!(s.get("/testcraft/1.0.0/.craft-release.json").status, 404);
    assert!(s.get("/").body.contains("Craft Apps"));
    let status: serde_json::Value = serde_json::from_str(&s.get("/status.json").body).unwrap();
    assert_eq!(status["apps"][0]["state"], "ready");

    // Configuration is mounted read-only; the root filesystem too.
    let out = s.compose(
        &["exec", "-T", "host", "/usr/local/bin/craft-host", "status"],
        &[],
    );
    assert!(out.status.success());

    // Update from /admin (form login) without restarting the server; old URLs keep working.
    let started = s.started_at();
    s.publish("1.1.0", &[]);
    let login = s.http("POST", "/auth/login", &[], Some("user=ana&password=s3cret"));
    assert_eq!(login.status, 303, "{}", login.body);
    let session = login
        .cookies
        .iter()
        .find(|c| c.starts_with("craft_session="))
        .unwrap()
        .clone();
    let page = s.http("GET", "/admin/", &[("Cookie", &session)], None);
    let csrf = page
        .cookies
        .iter()
        .find_map(|c| c.strip_prefix("craft_csrf="))
        .unwrap()
        .to_string();
    let cookies = format!("{session}; craft_csrf={csrf}");
    let r = s.http(
        "POST",
        "/admin/api/apps/testcraft/update",
        &[("Cookie", &cookies), ("X-Craft-CSRF", &csrf)],
        None,
    );
    assert_eq!(r.status, 202, "{}", r.body);
    s.wait_for("update from admin", || s.served() == "1.1.0/");
    assert_eq!(
        s.get("/testcraft/1.0.0/version.txt").body,
        "1.0.0",
        "open tabs keep their release"
    );
    assert_eq!(s.started_at(), started, "no restart");
    let st: serde_json::Value = serde_json::from_str(
        &s.http("GET", "/admin/api/status", &[("Cookie", &cookies)], None)
            .body,
    )
    .unwrap();
    assert_eq!(st["results"][0]["ok"], true, "{st}");

    // Simultaneous CLI updates are serialized by the lock: exactly one installs.
    s.publish("1.2.0", &[]);
    let spawn = || {
        s.command(
            &[
                "exec",
                "-T",
                "host",
                "/usr/local/bin/craft-host",
                "update",
                "testcraft",
            ],
            &[],
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
    };
    let (a, b) = (spawn(), spawn());
    let (a, b) = (a.wait_with_output().unwrap(), b.wait_with_output().unwrap());
    assert!(a.status.success() && b.status.success(), "{a:?} {b:?}");
    let outs = format!(
        "{}{}",
        String::from_utf8_lossy(&a.stdout),
        String::from_utf8_lossy(&b.stdout)
    );
    assert_eq!(
        outs.matches("installed and activated 1.2.0").count(),
        1,
        "{outs}"
    );
    assert_eq!(s.served(), "1.2.0/");

    // Disk exhaustion: WORK_DIR is a 1 MiB tmpfs; a 2 MiB incompressible release cannot fit.
    let noise: Vec<u8> = (0u32..(2 << 15))
        .flat_map(|i| Sha256::digest(i.to_le_bytes()))
        .collect();
    s.publish("1.3.0", &[("noise.bin", noise)]);
    let out = s.compose(
        &[
            "exec",
            "-T",
            "host",
            "/usr/local/bin/craft-host",
            "update",
            "testcraft",
        ],
        &[],
    );
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("insufficient disk space in WORK_DIR"),
        "{out:?}"
    );
    assert_eq!(
        s.served(),
        "1.2.0/",
        "a failed update keeps the working release"
    );

    // Offline restart: GitHub unreachable; installed releases are served immediately.
    s.ok(&["stop", "fakegh"]);
    s.ok(&["restart", "host"]);
    s.wait_for("server after restart", || s.get("/healthz").status == 200);
    assert_eq!(s.served(), "1.2.0/");
    let out = s.compose(
        &[
            "exec",
            "-T",
            "host",
            "/usr/local/bin/craft-host",
            "check",
            "testcraft",
        ],
        &[],
    );
    assert!(!out.status.success(), "check reports GitHub unreachable");
    assert_eq!(s.get("/readyz/testcraft").status, 200);
    s.ok(&["start", "fakegh"]);

    // A different UID works only through the shared group (directories are 2770, umask 0007).
    let gid = s.gid.to_string();
    let doctor = |envs: &[(&str, &str)]| {
        let out = s.compose(
            &["run", "--rm", "--no-deps", "host", "doctor", "--offline"],
            envs,
        );
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };
    let (ok, text) = doctor(&[("RUN_UID", "54321"), ("RUN_GID", "54321")]);
    assert!(!ok && text.contains("is not"), "{text}");
    let (ok, text) = doctor(&[
        ("RUN_UID", "54321"),
        ("RUN_GID", "54321"),
        ("SUPPLEMENTAL_GID", &gid),
    ]);
    assert!(ok, "{text}");
    let foreign = [
        ("RUN_UID", "54321"),
        ("RUN_GID", "54321"),
        ("SUPPLEMENTAL_GID", gid.as_str()),
    ];
    assert!(
        s.compose(
            &["up", "-d", "--no-deps", "--force-recreate", "host"],
            &foreign
        )
        .status
        .success()
    );
    s.wait_for("server as uid 54321", || s.get("/healthz").status == 200);
    s.publish("1.4.0", &[]);
    let out = s.compose(
        &[
            "exec",
            "-T",
            "host",
            "/usr/local/bin/craft-host",
            "update",
            "testcraft",
        ],
        &foreign,
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(s.served(), "1.4.0/");
    let meta = fs::metadata(s.base.join("data/releases/testcraft/1.4.0/version.txt")).unwrap();
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        (meta.uid(), meta.gid()),
        (54321, s.gid),
        "written by the configured UID, group inherited via setgid"
    );
}

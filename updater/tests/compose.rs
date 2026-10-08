//! Docker Compose integration test: the real nginx configuration, container identities,
//! read-only serving, updates without web restarts, concurrent CLI updates, offline restarts,
//! disk exhaustion and UID/GID compatibility. Uses a static fake GitHub on the project network
//! and host directories whose names contain spaces.
//!
//! Needs Docker with Compose v2: `cargo test --test compose -- --ignored --nocapture`.

use std::fs;
use std::io::Read;
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

impl Stack {
    fn compose(&self, args: &[&str], envs: &[(&str, &str)]) -> Output {
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
        let out = c.output().expect("docker compose");
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

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn get(&self, path: &str) -> (u16, String, String) {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .new_agent();
        let mut resp = agent.get(&self.url(path)).call().expect("http");
        let loc = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let mut body = String::new();
        let _ = resp
            .body_mut()
            .as_reader()
            .take(1 << 20)
            .read_to_string(&mut body);
        (resp.status().as_u16(), loc, body)
    }

    fn wait_for(&self, what: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        let _ = self.compose(&["logs", "--tail", "40"], &[]);
        panic!("timed out waiting for {what}");
    }

    fn fakegh(&self) -> PathBuf {
        self.base.join("fake github")
    }

    /// Add a release to the fake GitHub, newest first.
    fn publish(&self, version: &str, extra: &[(&str, Vec<u8>)]) {
        let top = format!("testcraft-web-{version}");
        let mut entries = craft_updater::testutil::site_entries(&top);
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
        craft_updater::testutil::deflated_zip(&dl.join(&name), &refs);
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
    }

    fn started_at(&self, service: &str) -> String {
        let id = self.ok(&["ps", "-q", service]).trim().to_string();
        let out = Command::new("docker")
            .args(["inspect", "-f", "{{.State.StartedAt}}", &id])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        let _ = self.compose(&["down", "--remove-orphans", "--timeout", "2"], &[]);
        let _ = craft_updater::fsutil::remove_tree(&self.base);
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
        // tempdir() creates 0700 trees; the shared group needs to enter DATA_DIR.
        fs::set_permissions(
            base.join(d),
            std::os::unix::fs::PermissionsExt::from_mode(0o750),
        )
        .unwrap();
    }
    let mut cfg = String::from(
        r#"
[updater]
github_api_url = "http://fakegh:8080"
max_retries = 1
retry_backoff = 0
heartbeat_interval = "5s"
[limits]
min_free_space = 0
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
    let port = free_port();
    let env_file = base.join("test.env");
    let d = |n: &str| base.join(n).display().to_string();
    fs::write(
        &env_file,
        format!(
            "RUN_UID={uid}\nRUN_GID={gid}\nFILE_UMASK=0022\nCONFIG_DIR={}\nDATA_DIR={}\nSTATE_DIR={}\nCACHE_DIR={}\nWORK_DIR={}\nFAKEGH_DIR={}\nWEB_BIND_ADDRESS=127.0.0.1\nWEB_PORT={port}\n",
            d("config"), d("data"), d("state"), d("cache"), d("work"), d("fake github")
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
    assert!(s.compose(&["build", "updater"], &[]).status.success());
    s.ok(&["up", "-d"]);
    // First start: installs through the private validation listener, then serves.
    s.wait_for("testcraft ready", || s.get("/readyz/testcraft").0 == 200);
    let (code, loc, _) = s.get("/testcraft/?webgl");
    assert_eq!((code, loc.as_str()), (302, "1.0.0/?webgl"));
    assert_eq!(s.get("/testcraft/1.0.0/version.txt").2, "1.0.0");
    assert_eq!(s.get("/testcraft/current/version.txt").0, 404);
    assert_eq!(s.get("/testcraft/1.0.0/.craft-release.json").0, 404);
    let status: serde_json::Value = serde_json::from_str(&s.get("/status.json").2).unwrap();
    assert_eq!(status["apps"][0]["state"], "ready");

    // Read-only serving and no access to updater internals from the web container.
    let out = s.compose(
        &["exec", "-T", "web", "sh", "-c", "touch /srv/data/public/x"],
        &[],
    );
    assert!(!out.status.success());
    let out = s.compose(
        &["exec", "-T", "web", "sh", "-c", "ls /srv/state /config"],
        &[],
    );
    assert!(
        !out.status.success(),
        "state and config are not mounted in the web container"
    );

    // Update without restarting the web server; the old release keeps serving open tabs.
    let web_started = s.started_at("web");
    s.publish("1.1.0", &[]);
    let out = s.ok(&[
        "exec",
        "-T",
        "updater",
        "craft-updater",
        "update",
        "testcraft",
    ]);
    assert!(out.contains("installed and activated 1.1.0"), "{out}");
    assert_eq!(s.get("/testcraft/").1, "1.1.0/");
    assert_eq!(s.get("/testcraft/1.0.0/version.txt").2, "1.0.0");
    assert_eq!(
        s.started_at("web"),
        web_started,
        "web server was not restarted"
    );

    // Simultaneous manual updates are serialized by the lock and leave a consistent result.
    s.publish("1.2.0", &[]);
    let spawn = || {
        Command::new("docker")
            .current_dir(&s.root)
            .args(["compose", "-p", &s.project, "--env-file"])
            .arg(&s.env_file)
            .args([
                "-f",
                "compose.yaml",
                "-f",
                "tests/integration/compose.test.yaml",
                "exec",
                "-T",
                "updater",
                "craft-updater",
                "update",
                "testcraft",
            ])
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
        "exactly one install: {outs}"
    );
    assert_eq!(s.get("/testcraft/").1, "1.2.0/");

    // Disk exhaustion: WORK_DIR is a 1 MiB tmpfs; a 2 MiB incompressible release cannot fit.
    let noise: Vec<u8> = (0u32..(2 << 15))
        .flat_map(|i| Sha256::digest(i.to_le_bytes()))
        .collect();
    s.publish("1.3.0", &[("noise.bin", noise)]);
    let out = s.compose(
        &[
            "exec",
            "-T",
            "updater",
            "craft-updater",
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
        s.get("/testcraft/").1,
        "1.2.0/",
        "failed update keeps the working release"
    );

    // Offline restart: GitHub unreachable; existing releases are served immediately.
    s.ok(&["stop", "fakegh"]);
    s.ok(&["restart", "web", "updater"]);
    s.wait_for("web after restart", || {
        std::panic::catch_unwind(|| s.get("/healthz").0 == 200).unwrap_or(false)
    });
    assert_eq!(s.get("/testcraft/").1, "1.2.0/");
    let out = s.compose(
        &[
            "exec",
            "-T",
            "updater",
            "craft-updater",
            "check",
            "testcraft",
        ],
        &[],
    );
    assert!(!out.status.success(), "check reports GitHub unreachable");
    assert_eq!(
        s.get("/readyz/testcraft").0,
        200,
        "updater failure does not affect readiness"
    );

    // UID/GID compatibility: a different web identity without a shared group cannot read
    // files published with umask 0027; doctor detects it, and a supplemental group fixes it.
    s.ok(&["start", "fakegh"]);
    s.publish("1.4.0", &[]);
    let out = s.compose(
        &[
            "exec",
            "-T",
            "-e",
            "FILE_UMASK=0027",
            "updater",
            "craft-updater",
            "update",
            "testcraft",
        ],
        &[],
    );
    assert!(out.status.success(), "{out:?}");
    let gid = s.gid.to_string();
    let doctor = |envs: &[(&str, &str)]| {
        let out = s.compose(
            &[
                "run",
                "--rm",
                "--no-deps",
                "-e",
                "FILE_UMASK=0027",
                "updater",
                "doctor",
                "--offline",
            ],
            envs,
        );
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };
    let (ok, text) = doctor(&[("WEB_UID", "54321"), ("WEB_GID", "54321")]);
    assert!(
        !ok && text.contains("cannot read files the updater publishes"),
        "{text}"
    );
    let (ok, text) = doctor(&[
        ("WEB_UID", "54321"),
        ("WEB_GID", "54321"),
        ("WEB_SUPPLEMENTAL_GID", &gid),
    ]);
    assert!(ok, "{text}");

    // nginx reads ./server and ./launcher from the checkout; a foreign identity needs them
    // world-readable (a restrictive umask at checkout time prevents that).
    for f in [
        "server/nginx.conf",
        "server/mime.types",
        "server/headers.conf",
        "launcher/index.html",
    ] {
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &fs::metadata(s.root.join(f)).unwrap().permissions(),
        );
        assert!(
            mode & 0o004 != 0,
            "{f} is not world-readable; run `chmod -R a+rX server launcher` (see docs/permissions.md)"
        );
    }
    let web_as = |envs: &[(&str, &str)]| {
        assert!(
            s.compose(&["up", "-d", "--no-deps", "--force-recreate", "web"], envs)
                .status
                .success()
        );
        s.wait_for("web up", || {
            std::panic::catch_unwind(|| s.get("/healthz").0 == 200).unwrap_or(false)
        });
        s.get("/testcraft/1.4.0/version.txt").0
    };
    // nginx answers 403 or 404 when it cannot read a path (try_files treats EACCES as missing).
    let denied = web_as(&[("WEB_UID", "54321"), ("WEB_GID", "54321")]);
    assert!(matches!(denied, 403 | 404), "unexpected status {denied}");
    assert_eq!(
        web_as(&[
            ("WEB_UID", "54321"),
            ("WEB_GID", "54321"),
            ("WEB_SUPPLEMENTAL_GID", &gid)
        ]),
        200
    );
}

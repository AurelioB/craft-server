//! A fake GitHub (releases API + asset downloads) and fixture helpers for lifecycle tests.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use craft_updater::config::{Config, Paths, load_config};

#[derive(Clone, Default)]
pub struct Behavior {
    pub status: Option<u16>,
    pub headers: Vec<(String, String)>,
    /// Send only this many body bytes, then close (interrupted transfer).
    pub truncate_at: Option<usize>,
    pub delay: Option<Duration>,
}

#[derive(Default)]
struct ServerState {
    releases: HashMap<String, Vec<Value>>,
    files: HashMap<String, Vec<u8>>,
    behaviors: HashMap<String, Behavior>,
    hits: HashMap<String, usize>,
}

pub struct FakeGitHub {
    pub url: String,
    state: Arc<Mutex<ServerState>>,
}

fn handle(mut stream: TcpStream, state: Arc<Mutex<ServerState>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
            break;
        }
    }
    let route = path.split('?').next().unwrap().to_string();
    let (status, body, behavior) = {
        let mut s = state.lock();
        *s.hits.entry(route.clone()).or_default() += 1;
        let behavior = s.behaviors.get(&route).cloned().unwrap_or_default();
        let (status, body) = if let Some(repo) = route
            .strip_prefix("/repos/")
            .and_then(|r| r.strip_suffix("/releases"))
        {
            match s.releases.get(repo) {
                Some(list) => (200, serde_json::to_vec(list).unwrap()),
                None => (404, b"{\"message\":\"Not Found\"}".to_vec()),
            }
        } else if let Some(f) = s.files.get(&route) {
            (200, f.clone())
        } else {
            (404, b"not found".to_vec())
        };
        (behavior.status.unwrap_or(status), body, behavior)
    };
    if let Some(d) = behavior.delay {
        std::thread::sleep(d);
    }
    let mut head = format!(
        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in &behavior.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let n = behavior.truncate_at.unwrap_or(body.len()).min(body.len());
    let _ = stream.write_all(&body[..n]);
    let _ = stream.flush();
}

impl FakeGitHub {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(ServerState::default()));
        let st = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let st = st.clone();
                std::thread::spawn(move || handle(stream, st));
            }
        });
        Self { url, state }
    }

    pub fn set_releases(&self, repo: &str, releases: Vec<Value>) {
        self.state.lock().releases.insert(repo.into(), releases);
    }

    pub fn put_file(&self, name: &str, data: Vec<u8>) -> String {
        let route = format!("/dl/{name}");
        self.state.lock().files.insert(route.clone(), data);
        format!("{}{route}", self.url)
    }

    pub fn behave(&self, route: &str, b: Behavior) {
        self.state.lock().behaviors.insert(route.into(), b);
    }

    pub fn clear_behaviors(&self) {
        self.state.lock().behaviors.clear();
    }

    pub fn hits(&self, route: &str) -> usize {
        self.state.lock().hits.get(route).copied().unwrap_or(0)
    }

    /// Publish a release with a web zip, SHA256SUMS.txt and API digests.
    pub fn publish(&self, repo: &str, tag: &str, zip: Vec<u8>, opts: PublishOpts) -> Value {
        let version = tag.trim_start_matches('v');
        let name = opts
            .asset_name
            .clone()
            .unwrap_or_else(|| format!("testcraft-web-{version}.zip"));
        let sha = hex_sha(&zip);
        let size = zip.len();
        let url = self.put_file(&format!("{tag}/{name}"), zip);
        let mut assets = vec![json!({
            "name": name, "size": size, "browser_download_url": url,
            "digest": if opts.api_digest { Value::String(format!("sha256:{}", opts.digest_override.clone().unwrap_or(sha.clone()))) } else { Value::Null },
        })];
        if opts.sums {
            let line = format!("{}  {name}\n", opts.sums_override.clone().unwrap_or(sha));
            let sums_url = self.put_file(&format!("{tag}/SHA256SUMS.txt"), line.into_bytes());
            assets.push(json!({"name": "SHA256SUMS.txt", "size": 1, "browser_download_url": sums_url, "digest": null}));
        }
        let release = json!({
            "tag_name": tag, "draft": false, "prerelease": opts.prerelease,
            "published_at": "2026-10-08T00:00:00Z", "html_url": "", "assets": assets,
        });
        let mut s = self.state.lock();
        let list = s.releases.entry(repo.into()).or_default();
        list.retain(|r| r["tag_name"] != tag);
        list.insert(0, release.clone());
        release
    }
}

#[derive(Clone)]
pub struct PublishOpts {
    pub asset_name: Option<String>,
    pub sums: bool,
    pub api_digest: bool,
    pub sums_override: Option<String>,
    pub digest_override: Option<String>,
    pub prerelease: bool,
}

impl Default for PublishOpts {
    fn default() -> Self {
        Self {
            asset_name: None,
            sums: true,
            api_digest: true,
            sums_override: None,
            digest_override: None,
            prerelease: false,
        }
    }
}

pub fn hex_sha(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A valid web release zip in the official layout, with an optional marker file.
pub fn release_zip(dir: &Path, version: &str, extra: &[(&str, &[u8])]) -> Vec<u8> {
    let top = format!("testcraft-web-{version}");
    let mut entries: Vec<(String, Vec<u8>)> = craft_updater::testutil::site_entries(&top);
    entries.push((format!("{top}/version.txt"), version.as_bytes().to_vec()));
    for (n, d) in extra {
        entries.push((format!("{top}/{n}"), d.to_vec()));
    }
    let p = dir.join(format!("{version}.zip"));
    let refs: Vec<(&str, &[u8])> = entries
        .iter()
        .map(|(n, d)| (n.as_str(), d.as_slice()))
        .collect();
    craft_updater::testutil::deflated_zip(&p, &refs);
    std::fs::read(&p).unwrap()
}

pub struct Env {
    pub dir: tempfile::TempDir,
    pub cfg: Config,
}

impl Env {
    pub fn root(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    pub fn release_dir(&self, version: &str) -> PathBuf {
        self.cfg.paths.data.join("releases/testcraft").join(version)
    }

    pub fn current(&self) -> Option<String> {
        std::fs::read_link(self.cfg.paths.data.join("releases/testcraft/current"))
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    }

    pub fn served_version(&self) -> Option<String> {
        std::fs::read_to_string(
            self.cfg
                .paths
                .public()
                .join("testcraft/current/version.txt"),
        )
        .ok()
    }

    pub fn paths_env(&self) -> Vec<(String, String)> {
        let p = &self.cfg.paths;
        vec![
            ("CRAFT_CONFIG".into(), p.config_file.display().to_string()),
            ("CRAFT_DATA_DIR".into(), p.data.display().to_string()),
            ("CRAFT_STATE_DIR".into(), p.state.display().to_string()),
            ("CRAFT_CACHE_DIR".into(), p.cache.display().to_string()),
            ("CRAFT_WORK_DIR".into(), p.work.display().to_string()),
        ]
    }
}

/// A test environment with directories whose names contain spaces and one custom app,
/// `testcraft`, served from the fake GitHub. `app_toml` adds keys to `[apps.testcraft]`,
/// `limits_toml` replaces the `[limits]` body and `sections` appends further tables.
pub fn env_with(gh: &FakeGitHub, app_toml: &str, limits_toml: &str, sections: &str) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("craft apps");
    let paths = Paths {
        config_file: base.join("config dir/config.toml"),
        data: base.join("data dir"),
        state: base.join("state dir"),
        cache: base.join("cache dir"),
        work: base.join("work dir"),
        logs: None,
    };
    for p in [
        &paths.data,
        &paths.state,
        &paths.cache,
        &paths.work,
        &paths.config_file.parent().unwrap().to_path_buf(),
    ] {
        std::fs::create_dir_all(p).unwrap();
    }
    let mut toml = format!(
        r#"
[updater]
github_api_url = "{}"
validation_url = ""
max_retries = 1
retry_backoff = 0
lock_wait = 0
[limits]
{limits_toml}
[apps.testcraft]
name = "TestCraft"
repository = "storytold/testcraft"
artifact_patterns = ["testcraft-web-{{version}}.zip", "oldcraft-web-{{version}}.zip"]
{app_toml}
"#,
        gh.url
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
        toml.push_str(&format!("[apps.{id}]\nenabled = false\n"));
    }
    toml.push_str(sections);
    std::fs::write(&paths.config_file, toml).unwrap();
    let cfg = load_config(paths).unwrap_or_else(|e| panic!("{e}"));
    Env { dir, cfg }
}

pub fn env(gh: &FakeGitHub) -> Env {
    env_with(gh, "", "min_free_space = 0", "")
}

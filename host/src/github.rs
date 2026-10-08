//! GitHub Releases discovery and bounded HTTP transfers with retries, backoff and rate-limit
//! handling.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{AppConfig, Channel, UpdaterSettings};
use crate::fsutil::{atomic_write_json, read_json};
use crate::timeutil::now_epoch;
use crate::version::{Version, parse_tag};

pub const USER_AGENT: &str = concat!("craft-apps-host/", env!("CARGO_PKG_VERSION"));
const CHUNK: usize = 1 << 16;

#[derive(Debug)]
pub enum NetError {
    /// GitHub asked us to wait; `reset_epoch` is when requests may resume.
    RateLimited {
        reset_epoch: i64,
        message: String,
    },
    TooLarge(String),
    Failed(String),
    /// Writing the download locally failed (for example, disk full); not retried.
    Local(std::io::Error),
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetError::RateLimited { message, .. } => write!(f, "{message}"),
            NetError::TooLarge(m) | NetError::Failed(m) => write!(f, "{m}"),
            NetError::Local(e) => write!(f, "local write failed: {e}"),
        }
    }
}

impl std::error::Error for NetError {}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReleaseAsset {
    pub name: String,
    #[serde(default)]
    pub size: u64,
    pub browser_download_url: String,
    #[serde(default)]
    pub digest: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Release {
    #[serde(default)]
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub assets: Vec<ReleaseAsset>,
}

/// An eligible release with exactly one matching web artifact.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub version: Version,
    pub tag: String,
    pub asset: ReleaseAsset,
    /// Hex SHA-256 reported by the GitHub API for the asset, if any.
    pub asset_digest: Option<String>,
    pub checksum_asset: Option<ReleaseAsset>,
    pub published_at: String,
}

fn artifact_regex(pattern: &str) -> Regex {
    let (before, after) = pattern.split_once("{version}").expect("validated pattern");
    Regex::new(&format!(
        "^{}(?P<v>.+){}$",
        regex::escape(before),
        regex::escape(after)
    ))
    .unwrap()
}

/// Eligible releases newest first, plus notes about releases that were skipped.
pub fn select_candidates(
    app: &AppConfig,
    releases: &[Release],
    allow_prerelease: bool,
) -> (Vec<Candidate>, Vec<String>) {
    let regexes: Vec<Regex> = app
        .artifact_patterns
        .iter()
        .map(|p| artifact_regex(p))
        .collect();
    let allow_pre = allow_prerelease || app.channel == Channel::Prerelease;
    let mut found = Vec::new();
    let mut notes = Vec::new();
    for rel in releases.iter().filter(|r| !r.draft) {
        let Some(version) = parse_tag(&rel.tag_name) else {
            notes.push(format!("{}: tag is not a release version", rel.tag_name));
            continue;
        };
        // GitHub's flag is not always set for rc builds (photocraft v0.1.1-rc.5), so a semver
        // pre-release suffix also counts as a pre-release.
        if (rel.prerelease || !version.pre.is_empty()) && !allow_pre {
            continue;
        }
        let text = version.to_string();
        let matches: Vec<&ReleaseAsset> = rel
            .assets
            .iter()
            .filter(|a| {
                regexes
                    .iter()
                    .any(|rx| rx.captures(&a.name).is_some_and(|c| c["v"] == text))
            })
            .collect();
        let asset = match matches.as_slice() {
            [] => {
                notes.push(format!(
                    "{}: no web artifact matching {}",
                    rel.tag_name,
                    app.artifact_patterns.join(", ")
                ));
                continue;
            }
            [one] => (*one).clone(),
            many => {
                let names: Vec<_> = many.iter().map(|a| a.name.as_str()).collect();
                notes.push(format!(
                    "{}: ambiguous web artifacts {}",
                    rel.tag_name,
                    names.join(", ")
                ));
                continue;
            }
        };
        let checksum_asset = rel
            .assets
            .iter()
            .find(|a| app.checksum_assets.contains(&a.name))
            .cloned();
        let asset_digest = asset
            .digest
            .as_deref()
            .and_then(|d| d.strip_prefix("sha256:"))
            .map(str::to_ascii_lowercase);
        found.push(Candidate {
            version,
            tag: rel.tag_name.clone(),
            asset,
            asset_digest,
            checksum_asset,
            published_at: rel.published_at.clone().unwrap_or_default(),
        });
    }
    found.sort_by(|a, b| b.version.cmp_precedence(&a.version));
    (found, notes)
}

/// Parse a `sha256sum`-style listing into (file name, lowercase hex) pairs.
pub fn parse_sha256sums(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let (hash, rest) = line.split_once(char::is_whitespace)?;
            let name = rest.trim_start().trim_start_matches('*');
            (hash.len() == 64 && hash.bytes().all(|c| c.is_ascii_hexdigit()) && !name.is_empty())
                .then(|| (name.to_string(), hash.to_ascii_lowercase()))
        })
        .collect()
}

#[derive(Serialize, Deserialize)]
struct CachedList {
    url: String,
    etag: String,
    releases: Vec<Release>,
}

pub struct GitHub {
    settings: UpdaterSettings,
    token: Option<String>,
    api_cache: PathBuf,
    api: ureq::Agent,
    downloads: ureq::Agent,
    pub sleep: Box<dyn Fn(Duration) + Send + Sync>,
}

enum Attempt<T> {
    Done(T),
    Retry(String),
    Fail(NetError),
}

fn agent(timeout_global: u64, connect: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(connect)))
        .timeout_global(Some(Duration::from_secs(timeout_global)))
        .user_agent(USER_AGENT)
        .build()
        .new_agent()
}

impl GitHub {
    pub fn new(settings: &UpdaterSettings, token: Option<String>, cache_dir: &Path) -> Self {
        Self {
            api: agent(settings.http_timeout_secs, settings.http_timeout_secs),
            downloads: agent(settings.download_timeout_secs, settings.http_timeout_secs),
            settings: settings.clone(),
            token,
            api_cache: cache_dir.join("api"),
            sleep: Box::new(std::thread::sleep),
        }
    }

    fn rate_limit(resp: &ureq::http::Response<ureq::Body>) -> Option<NetError> {
        let status = resp.status().as_u16();
        if status != 403 && status != 429 {
            return None;
        }
        let header = |k: &str| {
            resp.headers()
                .get(k)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        if let Some(secs) = header("retry-after").and_then(|v| v.parse::<i64>().ok()) {
            return Some(NetError::RateLimited {
                reset_epoch: now_epoch() + secs,
                message: format!("GitHub asked to retry after {secs}s"),
            });
        }
        if header("x-ratelimit-remaining").as_deref() == Some("0") {
            let reset = header("x-ratelimit-reset")
                .and_then(|v| v.parse().ok())
                .unwrap_or(now_epoch() + 3600);
            return Some(NetError::RateLimited {
                reset_epoch: reset,
                message: "GitHub API rate limit exhausted; configure github_token_file to raise it"
                    .into(),
            });
        }
        (status == 429).then(|| NetError::RateLimited {
            reset_epoch: now_epoch() + 60,
            message: "GitHub rate limited the request (HTTP 429)".into(),
        })
    }

    fn with_retries<T>(
        &self,
        what: &str,
        mut op: impl FnMut() -> Attempt<T>,
    ) -> Result<T, NetError> {
        let attempts = self.settings.max_retries + 1;
        let mut delay = self.settings.retry_backoff_secs;
        for attempt in 1..=attempts {
            match op() {
                Attempt::Done(v) => return Ok(v),
                Attempt::Fail(e) => return Err(e),
                Attempt::Retry(msg) => {
                    if attempt == attempts {
                        return Err(NetError::Failed(format!(
                            "{what}: {msg} (after {attempts} attempts)"
                        )));
                    }
                    log::warn!(
                        "{what}: {msg} (attempt {attempt}/{attempts}); retrying in {delay}s"
                    );
                    (self.sleep)(Duration::from_secs(delay));
                    delay = (delay * 2).min(self.settings.retry_backoff_max_secs);
                }
            }
        }
        unreachable!()
    }

    fn classify(&self, what: &str, resp: &ureq::http::Response<ureq::Body>) -> Option<Attempt<()>> {
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            return None;
        }
        if let Some(limited) = Self::rate_limit(resp) {
            return Some(Attempt::Fail(limited));
        }
        let msg = format!("HTTP {status}");
        Some(if status >= 500 || status == 408 {
            Attempt::Retry(msg)
        } else {
            Attempt::Fail(NetError::Failed(format!("{what}: {msg}")))
        })
    }

    /// List recent releases. Conditional requests make unchanged lists cost no API quota.
    pub fn list_releases(&self, repository: &str) -> Result<Vec<Release>, NetError> {
        let url = format!(
            "{}/repos/{repository}/releases?per_page=50",
            self.settings.github_api_url.trim_end_matches('/')
        );
        let cache_file = self
            .api_cache
            .join(format!("{}.json", repository.replace('/', "__")));
        let cached: Option<CachedList> = read_json(&cache_file)
            .ok()
            .flatten()
            .filter(|c: &CachedList| c.url == url);
        let what = format!("list releases of {repository}");
        let (etag, releases, fresh) = self.with_retries(&what, || {
            let mut req = self
                .api
                .get(&url)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28");
            if let Some(t) = &self.token {
                req = req.header("Authorization", format!("Bearer {t}"));
            }
            if let Some(c) = &cached {
                req = req.header("If-None-Match", &c.etag);
            }
            let mut resp = match req.call() {
                Ok(r) => r,
                Err(e) => return Attempt::Retry(e.to_string()),
            };
            if resp.status().as_u16() == 304
                && let Some(c) = &cached
            {
                return Attempt::Done((c.etag.clone(), c.releases.clone(), false));
            }
            if let Some(a) = self.classify(&what, &resp) {
                return match a {
                    Attempt::Retry(m) => Attempt::Retry(m),
                    Attempt::Fail(e) => Attempt::Fail(e),
                    Attempt::Done(()) => unreachable!(),
                };
            }
            let etag = resp
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let mut body = Vec::new();
            if let Err(e) = resp
                .body_mut()
                .as_reader()
                .take(16 << 20)
                .read_to_end(&mut body)
            {
                return Attempt::Retry(e.to_string());
            }
            match serde_json::from_slice::<Vec<Release>>(&body) {
                Ok(list) => Attempt::Done((etag, list, true)),
                Err(e) => Attempt::Retry(format!("unexpected response: {e}")),
            }
        })?;
        if fresh && !etag.is_empty() {
            let entry = CachedList {
                url: url.clone(),
                etag,
                releases: releases.clone(),
            };
            if let Err(e) = atomic_write_json(&cache_file, &entry) {
                log::warn!("cannot cache release list: {e:#}");
            }
        }
        Ok(releases)
    }

    pub fn fetch_small(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, NetError> {
        let what = format!("download {}", url.rsplit('/').next().unwrap_or(url));
        self.with_retries(&what, || {
            let mut resp = match self.api.get(url).call() {
                Ok(r) => r,
                Err(e) => return Attempt::Retry(e.to_string()),
            };
            if let Some(a) = self.classify(&what, &resp) {
                return match a {
                    Attempt::Retry(m) => Attempt::Retry(m),
                    Attempt::Fail(e) => Attempt::Fail(e),
                    Attempt::Done(()) => unreachable!(),
                };
            }
            let mut body = Vec::new();
            match resp
                .body_mut()
                .as_reader()
                .take(max_bytes + 1)
                .read_to_end(&mut body)
            {
                Ok(_) if body.len() as u64 > max_bytes => Attempt::Fail(NetError::TooLarge(
                    format!("{what}: larger than {max_bytes} bytes"),
                )),
                Ok(_) => Attempt::Done(body),
                Err(e) => Attempt::Retry(e.to_string()),
            }
        })
    }

    /// Stream `url` into `dest` (rewritten on each attempt). Returns (size, sha256 hex).
    pub fn download(
        &self,
        url: &str,
        dest: &mut File,
        max_bytes: u64,
    ) -> Result<(u64, String), NetError> {
        let what = format!("download {}", url.rsplit('/').next().unwrap_or(url));
        let overall = Duration::from_secs(self.settings.download_timeout_secs);
        self.with_retries(&what, || {
            if let Err(e) = dest.set_len(0).and_then(|_| dest.seek(SeekFrom::Start(0))) {
                return Attempt::Fail(NetError::Local(e));
            }
            let started = Instant::now();
            let mut resp = match self.downloads.get(url).call() {
                Ok(r) => r,
                Err(e) => return Attempt::Retry(e.to_string()),
            };
            if let Some(a) = self.classify(&what, &resp) {
                return match a {
                    Attempt::Retry(m) => Attempt::Retry(m),
                    Attempt::Fail(e) => Attempt::Fail(e),
                    Attempt::Done(()) => unreachable!(),
                };
            }
            let length = resp
                .headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            if length.is_some_and(|l| l > max_bytes) {
                return Attempt::Fail(NetError::TooLarge(format!(
                    "{what}: {} bytes, above the {max_bytes}-byte limit",
                    length.unwrap()
                )));
            }
            let mut reader = resp.body_mut().as_reader();
            let mut hasher = Sha256::new();
            let mut size = 0u64;
            let mut buf = vec![0u8; CHUNK];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) => {
                        return Attempt::Retry(format!(
                            "transfer interrupted after {size} bytes: {e}"
                        ));
                    }
                };
                size += n as u64;
                if size > max_bytes {
                    return Attempt::Fail(NetError::TooLarge(format!(
                        "{what}: exceeded the {max_bytes}-byte limit"
                    )));
                }
                if started.elapsed() > overall {
                    return Attempt::Retry(format!("exceeded {}s", overall.as_secs()));
                }
                hasher.update(&buf[..n]);
                if let Err(e) = dest.write_all(&buf[..n]) {
                    return Attempt::Fail(NetError::Local(e));
                }
            }
            if let Some(l) = length
                && l != size
            {
                return Attempt::Retry(format!("connection closed after {size} of {l} bytes"));
            }
            if let Err(e) = dest.sync_all() {
                return Attempt::Fail(NetError::Local(e));
            }
            Attempt::Done((size, hex(&hasher.finalize())))
        })
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, Channel};

    fn app(patterns: &[&str]) -> AppConfig {
        AppConfig {
            id: "pdfcraft".into(),
            name: "PdfCraft".into(),
            repository: "storytold/printcraft".into(),
            artifact_patterns: patterns.iter().map(|s| s.to_string()).collect(),
            checksum_assets: vec!["SHA256SUMS.txt".into()],
            entry: "pdfcraft".into(),
            release_dir: "releases/pdfcraft".into(),
            enabled: true,
            auto_update: true,
            pinned_version: None,
            channel: Channel::Stable,
            exclude: vec![],
            icon: String::new(),
            notes: String::new(),
            category: String::new(),
            tagline: String::new(),
            color: String::new(),
            keep_latest: 3,
            keep_days: 30,
            origin: None,
            activation: crate::config::Activation::Immediate,
            idle_after_secs: 1800,
        }
    }

    fn rel(tag: &str, pre: bool, assets: &[&str]) -> Release {
        Release {
            tag_name: tag.into(),
            draft: false,
            prerelease: pre,
            published_at: Some("2026-10-08T00:00:00Z".into()),
            html_url: String::new(),
            assets: assets
                .iter()
                .map(|n| ReleaseAsset {
                    name: n.to_string(),
                    size: 1,
                    browser_download_url: format!("https://x/{n}"),
                    digest: Some("sha256:AB".into()),
                })
                .collect(),
        }
    }

    #[test]
    fn renamed_artifacts_are_matched_and_prereleases_skipped() {
        let a = app(&["pdfcraft-web-{version}.zip", "printcraft-web-{version}.zip"]);
        let releases = vec![
            rel(
                "v0.4.0",
                false,
                &[
                    "pdfcraft-web-0.4.0.zip",
                    "pdfcraft-0.4.0-linux-x86_64.tar.gz",
                    "SHA256SUMS.txt",
                ],
            ),
            rel("v0.2.1", false, &["printcraft-web-0.2.1.zip"]),
            rel("v0.5.0-rc.1", false, &["pdfcraft-web-0.5.0-rc.1.zip"]),
            rel("v0.6.0", true, &["pdfcraft-web-0.6.0.zip"]),
            rel("v0.3.0", false, &["pdfcraft-web-0.2.9.zip"]),
            rel("nightly", false, &[]),
        ];
        let (found, notes) = select_candidates(&a, &releases, false);
        let tags: Vec<_> = found.iter().map(|c| c.tag.as_str()).collect();
        assert_eq!(tags, ["v0.4.0", "v0.2.1"]);
        assert_eq!(found[0].asset_digest.as_deref(), Some("ab"));
        assert!(found[0].checksum_asset.is_some());
        assert!(found[1].checksum_asset.is_none());
        assert!(
            notes
                .iter()
                .any(|n| n.starts_with("v0.3.0: no web artifact")),
            "{notes:?}"
        );
        assert!(notes.iter().any(|n| n.starts_with("nightly")), "{notes:?}");
        let (with_pre, _) = select_candidates(&a, &releases, true);
        assert_eq!(with_pre[0].tag, "v0.6.0");
    }

    #[test]
    fn ambiguous_artifacts_are_skipped() {
        let a = app(&["pdfcraft-web-{version}.zip", "printcraft-web-{version}.zip"]);
        let (found, notes) = select_candidates(
            &a,
            &[rel(
                "v1.0.0",
                false,
                &["pdfcraft-web-1.0.0.zip", "printcraft-web-1.0.0.zip"],
            )],
            false,
        );
        assert!(found.is_empty());
        assert!(notes[0].contains("ambiguous"));
    }

    #[test]
    fn sha256sums_parsing_accepts_binary_marker() {
        let h = "855da01e7ce518049c6f3a090f1218864f7b780811e340db409e5c3cef93cd6d";
        let text = format!(
            "{h}  photocraft-web-0.5.0.zip\n{}  *other.zip\nnot a line\n",
            h.to_uppercase()
        );
        let parsed = parse_sha256sums(&text);
        assert_eq!(
            parsed[0],
            ("photocraft-web-0.5.0.zip".to_string(), h.to_string())
        );
        assert_eq!(parsed[1].0, "other.zip");
        assert_eq!(parsed[1].1, h);
        assert_eq!(parsed.len(), 2);
    }
}

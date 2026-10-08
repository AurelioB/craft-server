//! Configuration: the built-in manifest, the operator's config.toml and container paths from the
//! environment. Every problem is collected so `doctor` can report them all at once.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use regex::Regex;
use serde::Deserialize;

use crate::version::parse_tag;

pub const MANIFEST: &str = include_str!("manifest.toml");
pub const PUBLIC_DIR: &str = "public";
pub const STAGING_DIR: &str = ".staging";
/// Paths the web server answers itself; an app entry must not shadow them.
pub const RESERVED_ENTRIES: &[&str] = &["launcher", "healthz", "readyz", "status", "candidate"];

#[derive(Debug)]
pub struct ConfigError(pub Vec<String>);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid configuration:")?;
        for p in &self.0 {
            write!(f, "\n  - {p}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config_file: PathBuf,
    pub data: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
    pub work: PathBuf,
    pub logs: Option<PathBuf>,
}

impl Paths {
    pub fn from_env() -> Self {
        let get = |k: &str, d: &str| {
            std::env::var(k)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| d.to_string())
        };
        Self {
            config_file: get("CRAFT_CONFIG", "/config/config.toml").into(),
            data: get("CRAFT_DATA_DIR", "/srv/data").into(),
            state: get("CRAFT_STATE_DIR", "/srv/state").into(),
            cache: get("CRAFT_CACHE_DIR", "/srv/cache").into(),
            work: get("CRAFT_WORK_DIR", "/srv/work").into(),
            logs: std::env::var("CRAFT_LOG_DIR")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(PathBuf::from),
        }
    }

    pub fn public(&self) -> PathBuf {
        self.data.join(PUBLIC_DIR)
    }

    pub fn staging(&self) -> PathBuf {
        self.data.join(STAGING_DIR)
    }

    pub fn config_dir(&self) -> PathBuf {
        self.config_file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/"))
    }
}

#[derive(Debug, Clone)]
pub struct UpdaterSettings {
    pub check_on_startup: bool,
    pub check_interval_secs: u64,
    pub http_timeout_secs: u64,
    pub download_timeout_secs: u64,
    pub max_retries: u32,
    pub retry_backoff_secs: u64,
    pub retry_backoff_max_secs: u64,
    pub github_api_url: String,
    pub github_token_file: String,
    pub validation_url: String,
    pub heartbeat_secs: u64,
    pub lock_wait_secs: u64,
    pub log_max_bytes: u64,
    pub log_backups: u32,
}

impl Default for UpdaterSettings {
    fn default() -> Self {
        Self {
            check_on_startup: true,
            check_interval_secs: 3600,
            http_timeout_secs: 30,
            download_timeout_secs: 900,
            max_retries: 3,
            retry_backoff_secs: 5,
            retry_backoff_max_secs: 300,
            github_api_url: "https://api.github.com".into(),
            github_token_file: String::new(),
            validation_url: "http://web:8081".into(),
            heartbeat_secs: 30,
            lock_wait_secs: 600,
            log_max_bytes: 10 << 20,
            log_backups: 5,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Limits {
    pub max_download_bytes: u64,
    pub max_extracted_bytes: u64,
    pub max_files: u64,
    pub max_compression_ratio: u64,
    pub min_free_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_download_bytes: 512 << 20,
            max_extracted_bytes: 1 << 30,
            max_files: 5000,
            max_compression_ratio: 200,
            min_free_bytes: 256 << 20,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Retention {
    pub keep_latest: u32,
    pub keep_days: u32,
    pub cache_max_bytes: u64,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            keep_latest: 3,
            keep_days: 30,
            cache_max_bytes: 2 << 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Channel {
    Stable,
    Prerelease,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub id: String,
    pub name: String,
    pub repository: String,
    pub artifact_patterns: Vec<String>,
    pub checksum_assets: Vec<String>,
    pub entry: String,
    pub release_dir: String,
    pub enabled: bool,
    pub auto_update: bool,
    pub pinned_version: Option<String>,
    pub channel: Channel,
    pub exclude: Vec<String>,
    pub icon: String,
    pub notes: String,
    pub keep_latest: u32,
    pub keep_days: u32,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub paths: Paths,
    pub updater: UpdaterSettings,
    pub limits: Limits,
    pub retention: Retention,
    pub apps: Vec<AppConfig>,
    pub config_file_found: bool,
}

impl Config {
    pub fn enabled_apps(&self) -> impl Iterator<Item = &AppConfig> {
        self.apps.iter().filter(|a| a.enabled)
    }

    pub fn app(&self, id: &str) -> Result<&AppConfig, ConfigError> {
        self.apps.iter().find(|a| a.id == id).ok_or_else(|| {
            let known: Vec<_> = self.apps.iter().map(|a| a.id.as_str()).collect();
            ConfigError(vec![format!(
                "unknown app {id:?}; known apps: {}",
                known.join(", ")
            )])
        })
    }

    pub fn release_root(&self, app: &AppConfig) -> PathBuf {
        self.paths.data.join(&app.release_dir)
    }

    pub fn token_path(&self) -> Option<PathBuf> {
        let name = self.updater.github_token_file.trim();
        if name.is_empty() {
            return None;
        }
        let p = PathBuf::from(name);
        Some(if p.is_absolute() {
            p
        } else {
            self.paths.config_dir().join(p)
        })
    }

    /// Read the optional GitHub token. The value is never logged or printed.
    pub fn read_token(&self) -> Result<Option<String>, ConfigError> {
        let Some(path) = self.token_path() else {
            return Ok(None);
        };
        match std::fs::read_to_string(&path) {
            Ok(t) => Ok(Some(t.trim().to_string()).filter(|t| !t.is_empty())),
            Err(e) => Err(ConfigError(vec![format!(
                "[updater] github_token_file: cannot read {} ({})",
                path.display(),
                e.kind()
            )])),
        }
    }
}

// ---- raw TOML ------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(untagged)]
enum Quantity {
    Int(i64),
    Text(String),
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawFile {
    updater: Option<RawUpdater>,
    limits: Option<RawLimits>,
    retention: Option<RawRetention>,
    #[serde(default)]
    apps: toml::Table,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawUpdater {
    check_on_startup: Option<bool>,
    check_interval: Option<Quantity>,
    http_timeout: Option<Quantity>,
    download_timeout: Option<Quantity>,
    max_retries: Option<u32>,
    retry_backoff: Option<Quantity>,
    retry_backoff_max: Option<Quantity>,
    github_api_url: Option<String>,
    github_token_file: Option<String>,
    validation_url: Option<String>,
    heartbeat_interval: Option<Quantity>,
    lock_wait: Option<Quantity>,
    log_max_size: Option<Quantity>,
    log_backups: Option<u32>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawLimits {
    max_download: Option<Quantity>,
    max_extracted: Option<Quantity>,
    max_files: Option<u64>,
    max_compression_ratio: Option<u64>,
    min_free_space: Option<Quantity>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawRetention {
    keep_latest: Option<u32>,
    keep_days: Option<u32>,
    cache_max_size: Option<Quantity>,
}

#[derive(Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
struct RawApp {
    // operator settings
    enabled: Option<bool>,
    auto_update: Option<bool>,
    pinned_version: Option<String>,
    channel: Option<String>,
    release_dir: Option<String>,
    entry: Option<String>,
    keep_latest: Option<u32>,
    keep_days: Option<u32>,
    // definition (built-in manifest, or operator-defined apps)
    name: Option<String>,
    repository: Option<String>,
    artifact_patterns: Option<Vec<String>>,
    checksum_assets: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    icon: Option<String>,
    notes: Option<String>,
}

impl RawApp {
    fn defines_app(&self) -> bool {
        self.name.is_some()
            || self.repository.is_some()
            || self.artifact_patterns.is_some()
            || self.checksum_assets.is_some()
            || self.exclude.is_some()
            || self.icon.is_some()
            || self.notes.is_some()
    }

    fn overlay(self, o: RawApp) -> RawApp {
        RawApp {
            enabled: o.enabled.or(self.enabled),
            auto_update: o.auto_update.or(self.auto_update),
            pinned_version: o.pinned_version.or(self.pinned_version),
            channel: o.channel.or(self.channel),
            release_dir: o.release_dir.or(self.release_dir),
            entry: o.entry.or(self.entry),
            keep_latest: o.keep_latest.or(self.keep_latest),
            keep_days: o.keep_days.or(self.keep_days),
            name: o.name.or(self.name),
            repository: o.repository.or(self.repository),
            artifact_patterns: o.artifact_patterns.or(self.artifact_patterns),
            checksum_assets: o.checksum_assets.or(self.checksum_assets),
            exclude: o.exclude.or(self.exclude),
            icon: o.icon.or(self.icon),
            notes: o.notes.or(self.notes),
        }
    }
}

fn parse_size(q: Option<Quantity>, key: &str, default: u64, problems: &mut Vec<String>) -> u64 {
    let re = Regex::new(r"(?i)^\s*(\d+)\s*(b|kb|mb|gb|tb|kib|mib|gib|tib)?\s*$").unwrap();
    match q {
        None => default,
        Some(Quantity::Int(n)) if n >= 0 => n as u64,
        Some(Quantity::Text(s)) if re.is_match(&s) => {
            let c = re.captures(&s).unwrap();
            let n: u64 = c[1].parse().unwrap_or(u64::MAX);
            let unit = c
                .get(2)
                .map(|m| m.as_str().to_ascii_lowercase())
                .unwrap_or_default();
            let mul: u64 = match unit.as_str() {
                "kb" => 1_000,
                "mb" => 1_000_000,
                "gb" => 1_000_000_000,
                "tb" => 1_000_000_000_000,
                "kib" => 1 << 10,
                "mib" => 1 << 20,
                "gib" => 1 << 30,
                "tib" => 1 << 40,
                _ => 1,
            };
            n.saturating_mul(mul)
        }
        Some(_) => {
            problems.push(format!(
                "{key}: expected bytes or a size such as \"512MiB\""
            ));
            default
        }
    }
}

fn parse_duration(q: Option<Quantity>, key: &str, default: u64, problems: &mut Vec<String>) -> u64 {
    let re = Regex::new(r"^\s*(\d+)\s*(s|m|h|d)?\s*$").unwrap();
    match q {
        None => default,
        Some(Quantity::Int(n)) if n >= 0 => n as u64,
        Some(Quantity::Text(s)) if re.is_match(&s) => {
            let c = re.captures(&s).unwrap();
            let n: u64 = c[1].parse().unwrap_or(u64::MAX);
            let mul = match c.get(2).map(|m| m.as_str()) {
                Some("m") => 60,
                Some("h") => 3600,
                Some("d") => 86_400,
                _ => 1,
            };
            n.saturating_mul(mul)
        }
        Some(_) => {
            problems.push(format!(
                "{key}: expected seconds or a duration such as \"1h\""
            ));
            default
        }
    }
}

pub fn valid_id(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

fn valid_dir_component(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || b"._- ".contains(c))
}

fn relative_components(rel: &str) -> Option<Vec<String>> {
    if rel.is_empty() || rel.starts_with('/') || rel.contains('\\') {
        return None;
    }
    let parts: Vec<String> = Path::new(rel)
        .components()
        .map(|c| match c {
            Component::Normal(s) => s.to_str().map(str::to_string),
            _ => None,
        })
        .collect::<Option<_>>()?;
    (!parts.is_empty()).then_some(parts)
}

fn build_app(
    id: &str,
    raw: RawApp,
    retention: &Retention,
    problems: &mut Vec<String>,
) -> Option<AppConfig> {
    let w = |k: &str| format!("[apps.{id}] {k}");
    let mut missing = false;
    for (key, present) in [
        ("name", raw.name.is_some()),
        ("repository", raw.repository.is_some()),
        (
            "artifact_patterns",
            raw.artifact_patterns
                .as_ref()
                .is_some_and(|v| !v.is_empty()),
        ),
    ] {
        if !present {
            problems.push(format!(
                "{}: required for apps that are not in the built-in manifest",
                w(key)
            ));
            missing = true;
        }
    }
    if missing {
        return None;
    }
    let repository = raw.repository.unwrap();
    let repo_re = Regex::new(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$").unwrap();
    if !repo_re.is_match(&repository) {
        problems.push(format!(
            "{}: expected \"owner/name\", got {repository:?}",
            w("repository")
        ));
    }
    let entry = raw.entry.unwrap_or_else(|| id.to_string());
    if !valid_id(&entry) || RESERVED_ENTRIES.contains(&entry.as_str()) {
        problems.push(format!(
            "{}: {entry:?} must be lowercase letters, digits and '-', and not one of {}",
            w("entry"),
            RESERVED_ENTRIES.join(", ")
        ));
    }
    let channel = match raw.channel.as_deref().unwrap_or("stable") {
        "stable" => Channel::Stable,
        "prerelease" => Channel::Prerelease,
        other => {
            problems.push(format!(
                "{}: expected \"stable\" or \"prerelease\", got {other:?}",
                w("channel")
            ));
            Channel::Stable
        }
    };
    let pinned_version = raw
        .pinned_version
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty());
    if let Some(p) = &pinned_version
        && parse_tag(p).is_none()
    {
        problems.push(format!(
            "{}: {p:?} is not a release version such as 0.5.0",
            w("pinned_version")
        ));
    }
    let release_dir = raw.release_dir.unwrap_or_else(|| format!("releases/{id}"));
    match relative_components(&release_dir) {
        Some(parts) if parts.iter().all(|p| valid_dir_component(p)) => {
            if parts[0] == PUBLIC_DIR {
                problems.push(format!("{}: must not be inside the published \"{PUBLIC_DIR}\" directory", w("release_dir")));
            }
        }
        _ => problems.push(format!(
            "{}: {release_dir:?} must be a relative path inside DATA_DIR whose components use letters, digits, '.', '_', '-' or spaces and do not start with '.'",
            w("release_dir")
        )),
    }
    let release_dir = relative_components(&release_dir)
        .map(|p| p.join("/"))
        .unwrap_or(release_dir);
    let patterns = raw.artifact_patterns.unwrap();
    for p in &patterns {
        if p.matches("{version}").count() != 1 || p.contains('/') {
            problems.push(format!(
                "{}: {p:?} must be a file name containing {{version}} exactly once",
                w("artifact_patterns")
            ));
        } else if !p.ends_with(".zip") {
            problems.push(format!(
                "{}: {p:?}: only .zip web archives are supported",
                w("artifact_patterns")
            ));
        }
    }
    let exclude = raw.exclude.unwrap_or_default();
    for p in &exclude {
        if relative_components(p.trim_end_matches("/**")).is_none() && !p.starts_with('*') {
            problems.push(format!(
                "{}: {p:?} must be a relative pattern without '..'",
                w("exclude")
            ));
        } else if ["*", "**", "**/*"].contains(&p.as_str()) || p.starts_with("index.html") {
            problems.push(format!(
                "{}: {p:?} would exclude the entry page",
                w("exclude")
            ));
        }
    }
    let icon = raw.icon.unwrap_or_default();
    if !icon.is_empty() && relative_components(&icon).is_none() {
        problems.push(format!(
            "{}: must be a relative path inside the release",
            w("icon")
        ));
    }
    Some(AppConfig {
        id: id.to_string(),
        name: raw.name.unwrap(),
        repository,
        artifact_patterns: patterns,
        checksum_assets: raw
            .checksum_assets
            .unwrap_or_else(|| vec!["SHA256SUMS.txt".into()]),
        entry,
        release_dir,
        enabled: raw.enabled.unwrap_or(true),
        auto_update: raw.auto_update.unwrap_or(true),
        pinned_version,
        channel,
        exclude,
        icon,
        notes: raw.notes.unwrap_or_default(),
        keep_latest: raw.keep_latest.unwrap_or(retention.keep_latest),
        keep_days: raw.keep_days.unwrap_or(retention.keep_days),
    })
}

fn check_overlaps(apps: &[AppConfig], problems: &mut Vec<String>) {
    let mut entries: BTreeMap<&str, &str> = BTreeMap::new();
    for a in apps {
        if let Some(other) = entries.insert(&a.entry, &a.id) {
            problems.push(format!(
                "[apps.{}] entry: {:?} is already used by {other}",
                a.id, a.entry
            ));
        }
    }
    for (i, a) in apps.iter().enumerate() {
        for b in &apps[i + 1..] {
            let (pa, pb): (Vec<_>, Vec<_>) = (
                a.release_dir.split('/').collect(),
                b.release_dir.split('/').collect(),
            );
            let n = pa.len().min(pb.len());
            if pa[..n] == pb[..n] {
                problems.push(format!(
                    "[apps.{}] and [apps.{}] release_dir overlap ({:?} vs {:?}); each app needs its own directory",
                    a.id, b.id, a.release_dir, b.release_dir
                ));
            }
        }
    }
}

fn check_roots(paths: &Paths, problems: &mut Vec<String>) {
    let mut roots: Vec<(&str, PathBuf)> = vec![
        ("DATA_DIR", paths.data.clone()),
        ("STATE_DIR", paths.state.clone()),
        ("CACHE_DIR", paths.cache.clone()),
        ("WORK_DIR", paths.work.clone()),
        ("CONFIG_DIR", paths.config_dir()),
    ];
    if let Some(l) = &paths.logs {
        roots.push(("LOG_DIR", l.clone()));
    }
    for (name, p) in &roots {
        if !p.is_absolute() {
            problems.push(format!(
                "{name}: container path {} must be absolute",
                p.display()
            ));
        }
    }
    for (i, (an, a)) in roots.iter().enumerate() {
        for (bn, b) in &roots[i + 1..] {
            // Logs kept inside the state directory are harmless.
            if *bn == "LOG_DIR" && *an == "STATE_DIR" && b.starts_with(a) && a != b {
                continue;
            }
            if a.starts_with(b) || b.starts_with(a) {
                problems.push(format!(
                    "{an} ({}) and {bn} ({}) overlap; they must be separate directories",
                    a.display(),
                    b.display()
                ));
            }
        }
    }
}

fn parse_apps_table(
    table: toml::Table,
    source: &str,
    problems: &mut Vec<String>,
) -> Vec<(String, RawApp)> {
    let mut out = Vec::new();
    for (id, value) in table {
        match value.try_into::<RawApp>() {
            Ok(raw) => out.push((id, raw)),
            Err(e) => problems.push(format!("{source} [apps.{id}]: {}", e.message())),
        }
    }
    out
}

pub fn load_config(paths: Paths) -> Result<Config, ConfigError> {
    let mut problems = Vec::new();
    let manifest: RawFile = toml::from_str(MANIFEST).expect("built-in manifest is valid");
    let manifest_apps = parse_apps_table(manifest.apps, "manifest", &mut problems);

    let (raw, found) = match std::fs::read_to_string(&paths.config_file) {
        Ok(text) => match toml::from_str::<RawFile>(&text) {
            Ok(raw) => (raw, true),
            Err(e) => {
                return Err(ConfigError(vec![format!(
                    "{}: {}",
                    paths.config_file.display(),
                    e.message()
                )]));
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (RawFile::default(), false),
        Err(e) => {
            return Err(ConfigError(vec![format!(
                "{}: cannot read ({})",
                paths.config_file.display(),
                e.kind()
            )]));
        }
    };

    let d = UpdaterSettings::default();
    let u = raw.updater.unwrap_or_default();
    let updater = UpdaterSettings {
        check_on_startup: u.check_on_startup.unwrap_or(d.check_on_startup),
        check_interval_secs: parse_duration(
            u.check_interval,
            "[updater] check_interval",
            d.check_interval_secs,
            &mut problems,
        ),
        http_timeout_secs: parse_duration(
            u.http_timeout,
            "[updater] http_timeout",
            d.http_timeout_secs,
            &mut problems,
        ),
        download_timeout_secs: parse_duration(
            u.download_timeout,
            "[updater] download_timeout",
            d.download_timeout_secs,
            &mut problems,
        ),
        max_retries: u.max_retries.unwrap_or(d.max_retries),
        retry_backoff_secs: parse_duration(
            u.retry_backoff,
            "[updater] retry_backoff",
            d.retry_backoff_secs,
            &mut problems,
        ),
        retry_backoff_max_secs: parse_duration(
            u.retry_backoff_max,
            "[updater] retry_backoff_max",
            d.retry_backoff_max_secs,
            &mut problems,
        ),
        github_api_url: u.github_api_url.unwrap_or(d.github_api_url),
        github_token_file: u.github_token_file.unwrap_or(d.github_token_file),
        validation_url: u.validation_url.unwrap_or(d.validation_url),
        heartbeat_secs: parse_duration(
            u.heartbeat_interval,
            "[updater] heartbeat_interval",
            d.heartbeat_secs,
            &mut problems,
        ),
        lock_wait_secs: parse_duration(
            u.lock_wait,
            "[updater] lock_wait",
            d.lock_wait_secs,
            &mut problems,
        ),
        log_max_bytes: parse_size(
            u.log_max_size,
            "[updater] log_max_size",
            d.log_max_bytes,
            &mut problems,
        ),
        log_backups: u.log_backups.unwrap_or(d.log_backups),
    };
    let dl = Limits::default();
    let l = raw.limits.unwrap_or_default();
    let limits = Limits {
        max_download_bytes: parse_size(
            l.max_download,
            "[limits] max_download",
            dl.max_download_bytes,
            &mut problems,
        ),
        max_extracted_bytes: parse_size(
            l.max_extracted,
            "[limits] max_extracted",
            dl.max_extracted_bytes,
            &mut problems,
        ),
        max_files: l.max_files.unwrap_or(dl.max_files),
        max_compression_ratio: l.max_compression_ratio.unwrap_or(dl.max_compression_ratio),
        min_free_bytes: parse_size(
            l.min_free_space,
            "[limits] min_free_space",
            dl.min_free_bytes,
            &mut problems,
        ),
    };
    let dr = Retention::default();
    let r = raw.retention.unwrap_or_default();
    let retention = Retention {
        keep_latest: r.keep_latest.unwrap_or(dr.keep_latest),
        keep_days: r.keep_days.unwrap_or(dr.keep_days),
        cache_max_bytes: parse_size(
            r.cache_max_size,
            "[retention] cache_max_size",
            dr.cache_max_bytes,
            &mut problems,
        ),
    };

    if updater.check_interval_secs < 60 {
        problems.push(
            "[updater] check_interval: must be at least 60 seconds to respect GitHub rate limits"
                .into(),
        );
    }
    if updater.max_retries > 10 {
        problems.push("[updater] max_retries: at most 10".into());
    }
    if updater.http_timeout_secs == 0
        || updater.download_timeout_secs == 0
        || updater.heartbeat_secs == 0
    {
        problems.push("[updater] timeouts and heartbeat_interval must be greater than zero".into());
    }
    if !updater.github_api_url.starts_with("https://")
        && !updater.github_api_url.starts_with("http://")
    {
        problems.push("[updater] github_api_url: must be an http(s) URL".into());
    }
    if !updater.validation_url.is_empty()
        && !updater.validation_url.starts_with("http://")
        && !updater.validation_url.starts_with("https://")
    {
        problems.push("[updater] validation_url: must be an http(s) URL, or empty to disable the serving check".into());
    }
    if limits.max_files == 0 || limits.max_compression_ratio == 0 {
        problems
            .push("[limits] max_files and max_compression_ratio must be greater than zero".into());
    }
    if retention.keep_latest == 0 {
        problems.push("[retention] keep_latest: must be at least 1".into());
    }

    let overrides = parse_apps_table(raw.apps, "config", &mut problems);
    let mut apps = Vec::new();
    let mut seen = Vec::new();
    for (id, base) in &manifest_apps {
        let over = overrides
            .iter()
            .find(|(o, _)| o == id)
            .map(|(_, r)| r.clone())
            .unwrap_or_default();
        if over.defines_app() {
            problems.push(format!(
                "[apps.{id}]: built-in apps accept only enabled, auto_update, pinned_version, channel, release_dir, entry, keep_latest and keep_days"
            ));
        }
        seen.push(id.clone());
        if let Some(app) = build_app(id, base.clone().overlay(over), &retention, &mut problems) {
            apps.push(app);
        }
    }
    for (id, raw_app) in overrides {
        if seen.contains(&id) {
            continue;
        }
        if !valid_id(&id) {
            problems.push(format!(
                "[apps.{id}]: app ids must be lowercase letters, digits and '-'"
            ));
            continue;
        }
        if let Some(app) = build_app(&id, raw_app, &retention, &mut problems) {
            apps.push(app);
        }
    }
    check_overlaps(&apps, &mut problems);
    check_roots(&paths, &mut problems);
    if !problems.is_empty() {
        return Err(ConfigError(problems));
    }
    Ok(Config {
        paths,
        updater,
        limits,
        retention,
        apps,
        config_file_found: found,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_with(config: &Path) -> Paths {
        Paths {
            config_file: config.to_path_buf(),
            data: "/srv/data".into(),
            state: "/srv/state".into(),
            cache: "/srv/cache".into(),
            work: "/srv/work".into(),
            logs: None,
        }
    }

    fn load(text: &str) -> Result<Config, ConfigError> {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        std::fs::write(&file, text).unwrap();
        let mut p = paths_with(&file);
        p.data = dir.path().join("data");
        p.state = dir.path().join("state");
        p.cache = dir.path().join("cache");
        p.work = dir.path().join("work");
        p.config_file = dir.path().join("config/config.toml");
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(&p.config_file, text).unwrap();
        load_config(p)
    }

    #[test]
    fn defaults_include_all_seven_apps_in_manifest_order() {
        let cfg = load("").unwrap();
        let ids: Vec<_> = cfg.apps.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "photocraft",
                "vectorcraft",
                "filmcraft",
                "lightcraft",
                "pdfcraft",
                "effectcraft",
                "designcraft"
            ]
        );
        let pdf = cfg.app("pdfcraft").unwrap();
        assert_eq!(pdf.repository, "storytold/printcraft");
        assert_eq!(pdf.release_dir, "releases/pdfcraft");
        assert_eq!(cfg.updater.check_interval_secs, 3600);
    }

    #[test]
    fn operator_overrides_and_units_apply() {
        let cfg = load(
            r#"
[updater]
check_interval = "2h"
[limits]
max_download = "64MiB"
[retention]
keep_latest = 5
[apps.photocraft]
enabled = false
release_dir = "my releases/photo"
pinned_version = "0.3.0"
"#,
        )
        .unwrap();
        assert_eq!(cfg.updater.check_interval_secs, 7200);
        assert_eq!(cfg.limits.max_download_bytes, 64 << 20);
        let p = cfg.app("photocraft").unwrap();
        assert!(!p.enabled);
        assert_eq!(p.release_dir, "my releases/photo");
        assert_eq!(p.pinned_version.as_deref(), Some("0.3.0"));
        assert_eq!(cfg.app("vectorcraft").unwrap().keep_latest, 5);
    }

    #[test]
    fn escaping_overlapping_and_reserved_settings_are_rejected() {
        let err = load(
            r#"
[apps.photocraft]
release_dir = "../outside"
[apps.vectorcraft]
release_dir = "public/vc"
[apps.filmcraft]
release_dir = "shared"
[apps.lightcraft]
release_dir = "shared/light"
entry = "healthz"
[apps.pdfcraft]
entry = "effectcraft"
"#,
        )
        .unwrap_err();
        let all = err.0.join("\n");
        assert!(all.contains("[apps.photocraft] release_dir"), "{all}");
        assert!(all.contains("published \"public\""), "{all}");
        assert!(
            all.contains("[apps.filmcraft] and [apps.lightcraft] release_dir overlap"),
            "{all}"
        );
        assert!(all.contains("[apps.lightcraft] entry"), "{all}");
        assert!(all.contains("\"effectcraft\" is already used"), "{all}");
    }

    #[test]
    fn unknown_keys_and_builtin_redefinitions_are_rejected() {
        let err = load("[updater]\ncheck_intervall = 60\n").unwrap_err();
        assert!(err.0[0].contains("unknown field"), "{:?}", err.0);
        let err = load("[apps.photocraft]\nrepository = \"evil/fork\"\n").unwrap_err();
        assert!(
            err.0.join("").contains("built-in apps accept only"),
            "{:?}",
            err.0
        );
    }

    #[test]
    fn custom_apps_need_a_definition() {
        let err = load("[apps.newcraft]\nenabled = true\n").unwrap_err();
        assert!(
            err.0
                .join("\n")
                .contains("[apps.newcraft] repository: required"),
            "{:?}",
            err.0
        );
        let cfg = load(
            "[apps.newcraft]\nname = \"NewCraft\"\nrepository = \"storytold/newcraft\"\nartifact_patterns = [\"newcraft-web-{version}.zip\"]\n",
        )
        .unwrap();
        assert_eq!(cfg.app("newcraft").unwrap().entry, "newcraft");
    }

    #[test]
    fn overlapping_root_directories_are_rejected() {
        let mut p = paths_with(Path::new("/config/config.toml"));
        p.cache = "/srv/data/cache".into();
        let err = load_config(p).unwrap_err();
        assert!(
            err.0
                .iter()
                .any(|e| e.contains("DATA_DIR") && e.contains("CACHE_DIR")),
            "{:?}",
            err.0
        );
    }
}

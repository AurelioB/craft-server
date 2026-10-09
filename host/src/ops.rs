//! The update lifecycle and operator operations (pin, rollback, allow, retention).
//! Every function that changes state expects the caller to hold the operation lock.

use std::cell::OnceCell;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

use crate::activity::Activity;
use crate::archive;
use crate::candidate;
use crate::config::{Activation, AppConfig, Config};
use crate::fsutil::{
    FILE_MODE, free_bytes, make_dirs, move_file, random_hex, remove_tree, strip_write_bits,
};
use crate::github::{Candidate, GitHub, NetError, hex, parse_sha256sums, select_candidates};
use crate::layout;
use crate::precompress;
use crate::store::{AppState, Failure, InstalledRelease, SeenRelease, Store};
use crate::timeutil::{now_epoch, now_iso, parse_iso};
use crate::validate::{self, ValidationError};
use crate::version::{cmp_text, parse_tag};

/// Failure of one stage of an update; `transient` failures are retried sooner.
#[derive(Debug, Clone)]
pub struct StageError {
    pub stage: &'static str,
    pub message: String,
    pub transient: bool,
    pub retry_at: Option<i64>,
}

impl StageError {
    fn new(stage: &'static str, message: impl Into<String>) -> Self {
        Self {
            stage,
            message: message.into(),
            transient: false,
            retry_at: None,
        }
    }

    fn transient(stage: &'static str, message: impl Into<String>) -> Self {
        Self {
            stage,
            message: message.into(),
            transient: true,
            retry_at: None,
        }
    }

    fn net(stage: &'static str, e: NetError) -> Self {
        match e {
            NetError::RateLimited {
                reset_epoch,
                message,
            } => Self {
                stage,
                message,
                transient: true,
                retry_at: Some(reset_epoch),
            },
            NetError::Local(err) => Self::new(stage, local_message(&err)),
            NetError::TooLarge(m) => Self::new(stage, m),
            NetError::Failed(m) => Self::transient(stage, m),
        }
    }
}

fn local_message(err: &io::Error) -> String {
    match err.kind() {
        io::ErrorKind::StorageFull => format!("insufficient disk space ({err})"),
        io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem => {
            format!("permission denied ({err}); run `doctor`")
        }
        _ => err.to_string(),
    }
}

fn io_stage(stage: &'static str) -> impl Fn(anyhow::Error) -> StageError {
    move |e| match e.downcast_ref::<io::Error>() {
        Some(io) => StageError::new(stage, format!("{}: {e:#}", local_message(io))),
        None => match e.downcast_ref::<archive::UnsafeArchive>() {
            Some(u) => StageError::new("archive", u.0.clone()),
            None => StageError::new(stage, format!("{e:#}")),
        },
    }
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Installed(String),
    Activated(String),
    /// Installed; becomes active once the app is idle (activation = "idle").
    Staged(String),
    UpToDate(Option<String>),
    Skipped(String),
    Failed(StageError),
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Installed(v) => write!(f, "installed and activated {v}"),
            Outcome::Activated(v) => write!(f, "activated installed release {v}"),
            Outcome::Staged(v) => {
                write!(f, "installed {v}; it becomes active once the app is idle")
            }
            Outcome::UpToDate(Some(v)) => write!(f, "up to date ({v})"),
            Outcome::UpToDate(None) => write!(f, "nothing to do"),
            Outcome::Skipped(why) => write!(f, "skipped: {why}"),
            Outcome::Failed(e) => write!(f, "failed during {}: {}", e.stage, e.message),
        }
    }
}

pub struct CheckReport {
    pub active: Option<String>,
    pub latest: Option<String>,
    pub update_available: bool,
    pub notes: Vec<String>,
}

pub struct Updater<'a> {
    pub cfg: &'a Config,
    pub store: Store,
    gh: OnceCell<GitHub>,
    /// Request activity, available inside the serving process only.
    activity: Option<Arc<Activity>>,
}

impl<'a> Updater<'a> {
    pub fn new(cfg: &'a Config) -> Self {
        Self {
            cfg,
            store: Store::new(&cfg.paths.state),
            gh: OnceCell::new(),
            activity: None,
        }
    }

    pub fn with_activity(mut self, activity: Arc<Activity>) -> Self {
        self.activity = Some(activity);
        self
    }

    /// Use a preconfigured client (tests, or to override sleeping between retries).
    pub fn with_github(cfg: &'a Config, gh: GitHub) -> Self {
        let u = Self::new(cfg);
        let _ = u.gh.set(gh);
        u
    }

    fn gh(&self) -> Result<&GitHub, StageError> {
        if let Some(g) = self.gh.get() {
            return Ok(g);
        }
        let token = self
            .cfg
            .read_token()
            .map_err(|e| StageError::new("config", e.0.join("; ")))?;
        Ok(self
            .gh
            .get_or_init(|| GitHub::new(&self.cfg.updater, token, &self.cfg.paths.cache)))
    }

    /// Create the persistent layout. Idempotent.
    pub fn prepare(&self) -> Result<()> {
        self.store.ensure()?;
        layout::ensure_base(self.cfg)?;
        for app in self.cfg.enabled_apps() {
            layout::ensure_app(self.cfg, app)?;
        }
        Ok(())
    }

    /// Whether the app has been idle long enough for idle activation. Without activity data
    /// (a CLI process) this is unknown and treated as busy; the server process decides.
    fn is_idle(&self, app: &AppConfig) -> bool {
        self.activity
            .as_ref()
            .is_some_and(|a| a.idle_secs(&app.id) >= app.idle_after_secs as i64)
    }

    /// Decide whether an installed release goes live now or waits for the app to be idle.
    fn wants_staging(&self, app: &AppConfig, state: &AppState, manual: bool) -> bool {
        !manual
            && app.activation == Activation::Idle
            && state.active.is_some()
            && !self.is_idle(app)
    }

    fn effective_pin(&self, app: &AppConfig, state: &AppState) -> Option<String> {
        state
            .pinned
            .clone()
            .or_else(|| app.pinned_version.clone())
            .map(|p| p.trim_start_matches('v').to_string())
    }

    fn discover(
        &self,
        app: &AppConfig,
        state: &mut AppState,
        include_prerelease: bool,
    ) -> Result<(Vec<Candidate>, Vec<Candidate>), StageError> {
        let releases = self
            .gh()?
            .list_releases(&app.repository)
            .map_err(|e| StageError::net("discover", e));
        state.last_check = Some(now_iso());
        let releases = match releases {
            Ok(r) => r,
            Err(e) => {
                state.last_check_ok = Some(false);
                return Err(e);
            }
        };
        state.last_check_ok = Some(true);
        let (eligible, notes) = select_candidates(app, &releases, false);
        let all = if include_prerelease {
            select_candidates(app, &releases, true).0
        } else {
            Vec::new()
        };
        state.skipped = notes.into_iter().take(10).collect();
        state.latest_seen = eligible.first().map(|c| SeenRelease {
            version: c.version.to_string(),
            tag: c.tag.clone(),
            published_at: c.published_at.clone(),
        });
        Ok((eligible, all))
    }

    /// Discover the newest eligible release without installing anything.
    pub fn check(&self, app: &AppConfig) -> Result<CheckReport> {
        let mut state = self.store.load(&app.id)?;
        let result = self.discover(app, &mut state, false);
        if let Err(e) = &result {
            state.last_error = Some(Failure {
                at: now_iso(),
                stage: e.stage.into(),
                message: e.message.clone(),
                version: None,
            });
        }
        self.store.save(&app.id, &state)?;
        let (eligible, _) = result.map_err(|e| anyhow!("{}: {}", e.stage, e.message))?;
        let latest = eligible.first().map(|c| c.version.to_string());
        let update_available = match (&latest, &state.active) {
            (Some(l), Some(a)) => cmp_text(l, a).is_gt() && !state.blocked.contains(l),
            (Some(_), None) => true,
            _ => false,
        };
        Ok(CheckReport {
            active: state.active.clone(),
            latest,
            update_available,
            notes: state.skipped.clone(),
        })
    }

    /// Bring one app to its desired release. `manual` bypasses failure backoff and
    /// `auto_update = false`.
    pub fn update_app(&self, app: &AppConfig, manual: bool) -> Outcome {
        match self.update_app_inner(app, manual) {
            Ok(o) => o,
            Err(e) => Outcome::Failed(StageError::new("state", format!("{e:#}"))),
        }
    }

    fn update_app_inner(&self, app: &AppConfig, manual: bool) -> Result<Outcome> {
        let mut state = self.store.load(&app.id)?;
        let now = now_epoch();
        if !manual && let Some(at) = state.retry_after.filter(|t| *t > now) {
            return Ok(Outcome::Skipped(format!(
                "waiting until {} after earlier failures",
                crate::timeutil::iso(at)
            )));
        }
        let pin = self.effective_pin(app, &state);
        let outcome = match self.discover(app, &mut state, pin.is_some()) {
            Err(e) => {
                // Offline or rate limited: an installed pin can still be activated.
                if let Some(p) = pin.as_ref().filter(|p| state.installed.contains_key(*p)) {
                    self.activate_installed(app, &mut state, p, true)?
                } else {
                    Outcome::Failed(e)
                }
            }
            Ok((eligible, all)) => match pin {
                Some(p) => {
                    if state.installed.contains_key(&p) {
                        self.activate_installed(app, &mut state, &p, true)?
                    } else if let Some(c) = all.iter().find(|c| c.version.to_string() == p) {
                        self.install_and_activate(app, &mut state, c, true)?
                    } else {
                        Outcome::Failed(StageError::new(
                            "discover",
                            format!(
                                "pinned version {p} has no matching web artifact among recent releases"
                            ),
                        ))
                    }
                }
                None => self.follow_latest(app, &mut state, &eligible, manual)?,
            },
        };
        match &outcome {
            Outcome::Failed(e) => {
                state.consecutive_failures += 1;
                let backoff =
                    60u64.saturating_mul(1 << state.consecutive_failures.min(10).saturating_sub(1));
                let delay = backoff.min(self.cfg.updater.check_interval_secs) as i64;
                state.retry_after = Some(e.retry_at.unwrap_or(now + delay));
                state.last_error = Some(Failure {
                    at: now_iso(),
                    stage: e.stage.into(),
                    message: e.message.clone(),
                    version: None,
                });
                self.store.record(
                    &app.id,
                    "update",
                    "failed",
                    None,
                    &format!("{}: {}", e.stage, e.message),
                )?;
            }
            _ => {
                state.consecutive_failures = 0;
                state.retry_after = None;
                if !matches!(outcome, Outcome::Skipped(_)) {
                    state.last_error = None;
                }
            }
        }
        self.store.save(&app.id, &state)?;
        if matches!(
            outcome,
            Outcome::Installed(_) | Outcome::Activated(_) | Outcome::Staged(_)
        ) {
            self.retain_logged(app);
        }
        Ok(outcome)
    }

    fn follow_latest(
        &self,
        app: &AppConfig,
        state: &mut AppState,
        eligible: &[Candidate],
        manual: bool,
    ) -> Result<Outcome> {
        let Some(target) = eligible.first() else {
            return Ok(match &state.active {
                Some(a) => Outcome::UpToDate(Some(a.clone())),
                None => Outcome::Failed(StageError::new(
                    "discover",
                    "no eligible release with a web artifact",
                )),
            });
        };
        let v = target.version.to_string();
        if let Some(active) = &state.active {
            if state.blocked.contains(&v) {
                return Ok(Outcome::Skipped(format!(
                    "{v} is blocked after a rollback; `allow {} {v}` to permit it",
                    app.id
                )));
            }
            if !cmp_text(&v, active).is_gt() {
                return Ok(Outcome::UpToDate(Some(active.clone())));
            }
            if !manual && !app.auto_update {
                return Ok(Outcome::Skipped(format!(
                    "{v} is available; automatic updates are disabled for this app"
                )));
            }
        }
        if state.installed.contains_key(&v) {
            return self.activate_installed(app, state, &v, manual);
        }
        self.install_and_activate(app, state, target, manual)
    }

    fn activate_installed(
        &self,
        app: &AppConfig,
        state: &mut AppState,
        version: &str,
        manual: bool,
    ) -> Result<Outcome> {
        if state.active.as_deref() == Some(version)
            && layout::active_version(self.cfg, app).as_deref() == Some(version)
        {
            state.pending = None;
            return Ok(Outcome::UpToDate(Some(version.to_string())));
        }
        if self.wants_staging(app, state, manual) {
            self.stage(app, state, version)?;
            return Ok(Outcome::Staged(version.to_string()));
        }
        self.go_live(app, state, version, "")?;
        Ok(Outcome::Activated(version.to_string()))
    }

    /// Leave an installed release waiting for idle activation (recorded once).
    fn stage(&self, app: &AppConfig, state: &mut AppState, version: &str) -> Result<()> {
        if state.pending.as_deref() != Some(version) {
            state.pending = Some(version.to_string());
            self.store.record(
                &app.id,
                "stage",
                "waiting",
                Some(version),
                "activates when the app is idle",
            )?;
        }
        Ok(())
    }

    /// Switch the active-version pointer and record it.
    fn go_live(
        &self,
        app: &AppConfig,
        state: &mut AppState,
        version: &str,
        note: &str,
    ) -> Result<()> {
        layout::activate(self.cfg, app, version)?;
        state.active = Some(version.to_string());
        state.pending = None;
        state.last_update = Some(now_iso());
        self.store
            .record(&app.id, "activate", "ok", Some(version), note)
    }

    fn install_and_activate(
        &self,
        app: &AppConfig,
        state: &mut AppState,
        c: &Candidate,
        manual: bool,
    ) -> Result<Outcome> {
        let v = c.version.to_string();
        log::info!("{}: installing {v} from {}", app.id, c.asset.name);
        match self.install(app, c) {
            Ok(record) => {
                state.installed.insert(v.clone(), record);
                state.failed_versions.remove(&v);
                self.store
                    .record(&app.id, "install", "ok", Some(&v), &c.asset.name)?;
                if self.wants_staging(app, state, manual) {
                    self.stage(app, state, &v)?;
                    return Ok(Outcome::Staged(v));
                }
                self.go_live(app, state, &v, "")?;
                Ok(Outcome::Installed(v))
            }
            Err(e) => {
                state.failed_versions.insert(
                    v.clone(),
                    Failure {
                        at: now_iso(),
                        stage: e.stage.into(),
                        message: e.message.clone(),
                        version: Some(v.clone()),
                    },
                );
                self.store.record(
                    &app.id,
                    "install",
                    "failed",
                    Some(&v),
                    &format!("{}: {}", e.stage, e.message),
                )?;
                Ok(Outcome::Failed(StageError {
                    message: format!("{v}: {}", e.message),
                    ..e
                }))
            }
        }
    }

    /// Retention after a successful change: housekeeping, so it never turns the change into a
    /// failure.
    fn retain_logged(&self, app: &AppConfig) {
        if let Err(e) = self.apply_retention(app) {
            log::warn!("{}: retention failed: {e:#}", app.id);
        }
    }

    /// Activate a pending release if its app is idle (or `force`). Returns the activated version.
    pub fn apply_pending(&self, app: &AppConfig, force: bool) -> Result<Option<String>> {
        let mut state = self.store.load(&app.id)?;
        let Some(v) = state.pending.clone() else {
            return Ok(None);
        };
        if !force && app.activation == Activation::Idle && !self.is_idle(app) {
            return Ok(None);
        }
        let note = if force {
            "applied by operator"
        } else {
            "applied while idle"
        };
        self.go_live(app, &mut state, &v, note)?;
        self.store.save(&app.id, &state)?;
        self.retain_logged(app);
        Ok(Some(v))
    }

    fn expected_sha256(&self, c: &Candidate) -> Result<String, StageError> {
        let mut from_sums = None;
        if let Some(sums) = &c.checksum_asset {
            let body = self
                .gh()?
                .fetch_small(&sums.browser_download_url, 1 << 20)
                .map_err(|e| StageError::net("checksum", e))?;
            from_sums = parse_sha256sums(&String::from_utf8_lossy(&body))
                .into_iter()
                .find(|(n, _)| *n == c.asset.name)
                .map(|(_, h)| h);
        }
        match (from_sums, &c.asset_digest) {
            (Some(a), Some(b)) if a != *b => Err(StageError::new(
                "checksum",
                format!(
                    "{} and the GitHub asset digest disagree for {}",
                    c.checksum_asset.as_ref().unwrap().name,
                    c.asset.name
                ),
            )),
            (Some(a), _) => Ok(a),
            (None, Some(b)) => Ok(b.clone()),
            (None, None) => Err(StageError::new(
                "checksum",
                format!("no published SHA-256 for {}; release skipped", c.asset.name),
            )),
        }
    }

    fn ensure_space(&self, dir: &Path, needed: u64, what: &str) -> Result<(), StageError> {
        let free = free_bytes(dir).map_err(|e| {
            StageError::new("space", format!("cannot measure free space of {what}: {e}"))
        })?;
        let want = needed.saturating_add(self.cfg.limits.min_free_bytes);
        if free < want {
            return Err(StageError::new(
                "space",
                format!(
                    "insufficient disk space in {what}: {free} bytes free, {want} needed (including min_free_space)"
                ),
            ));
        }
        Ok(())
    }

    /// Download (or reuse) a verified archive in CACHE_DIR.
    fn obtain_archive(
        &self,
        app: &AppConfig,
        c: &Candidate,
        expected: &str,
    ) -> Result<PathBuf, StageError> {
        let limits = &self.cfg.limits;
        if c.asset.size > limits.max_download_bytes {
            return Err(StageError::new(
                "limits",
                format!(
                    "{} is {} bytes, above max_download ({})",
                    c.asset.name, c.asset.size, limits.max_download_bytes
                ),
            ));
        }
        let cache_dir = self.cfg.paths.cache.join("archives").join(&app.id);
        make_dirs(&cache_dir).map_err(|e| StageError::new("download", local_message(&e)))?;
        let cached = cache_dir.join(&c.asset.name);
        if cached.is_file() {
            if sha256_file(&cached).ok().as_deref() == Some(expected) {
                log::info!("{}: reusing verified download {}", app.id, c.asset.name);
                let _ = fs::File::open(&cached)
                    .and_then(|f| f.set_modified(std::time::SystemTime::now()));
                return Ok(cached);
            }
            let _ = fs::remove_file(&cached);
        }
        let downloads = self.cfg.paths.work.join("downloads");
        make_dirs(&downloads).map_err(|e| StageError::new("download", local_message(&e)))?;
        self.ensure_space(&downloads, c.asset.size, "WORK_DIR")?;
        let part = downloads.join(format!("{}.{}.part", c.asset.name, random_hex(6)));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .open(&part)
            .map_err(|e| StageError::new("download", local_message(&e)))?;
        let result = self.gh()?.download(
            &c.asset.browser_download_url,
            &mut file,
            limits.max_download_bytes,
        );
        drop(file);
        let (size, sha) = match result {
            Ok(r) => r,
            Err(e) => {
                let _ = fs::remove_file(&part);
                return Err(StageError::net("download", e));
            }
        };
        if sha != expected {
            let _ = fs::remove_file(&part);
            return Err(StageError::new(
                "checksum",
                format!(
                    "SHA-256 mismatch for {} ({size} bytes): expected {expected}, got {sha}",
                    c.asset.name
                ),
            ));
        }
        if let Err(e) = move_file(&part, &cached) {
            let _ = fs::remove_file(&part);
            return Err(io_stage("download")(e));
        }
        Ok(cached)
    }

    fn install(&self, app: &AppConfig, c: &Candidate) -> Result<InstalledRelease, StageError> {
        let version = c.version.to_string();
        if !layout::valid_release_name(&version) {
            return Err(StageError::new(
                "discover",
                format!("version {version:?} cannot be used as a directory name"),
            ));
        }
        let expected = self.expected_sha256(c)?;
        let archive_path = self.obtain_archive(app, c, &expected)?;
        layout::ensure_app(self.cfg, app).map_err(io_stage("publish"))?;
        let mut zip = archive::open(&archive_path).map_err(|e| StageError::new("archive", e.0))?;
        let plan = archive::plan_extraction(&mut zip, &app.exclude, &self.cfg.limits)
            .map_err(|e| StageError::new("archive", e.0))?;
        if !plan.excluded.is_empty() {
            log::info!(
                "{}: excluded {} archive entries by manifest rules",
                app.id,
                plan.excluded.len()
            );
        }
        self.ensure_space(&self.cfg.paths.data, plan.total_bytes, "DATA_DIR")?;
        let staging = layout::new_staging(self.cfg, app, &version).map_err(io_stage("extract"))?;
        let result = self.stage_and_publish(app, c, &version, &expected, &mut zip, &plan, &staging);
        let _ = remove_tree(&staging);
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn stage_and_publish(
        &self,
        app: &AppConfig,
        c: &Candidate,
        version: &str,
        sha256: &str,
        zip: &mut zip::ZipArchive<fs::File>,
        plan: &archive::Plan,
        staging: &Path,
    ) -> Result<InstalledRelease, StageError> {
        let site = staging.join("site");
        fs::create_dir(&site).map_err(|e| StageError::new("extract", local_message(&e)))?;
        let written =
            archive::extract(zip, plan, &site, &self.cfg.limits).map_err(io_stage("extract"))?;
        let report = validate::validate_tree(&site, &app.icon, self.cfg.limits.max_extracted_bytes)
            .map_err(|e| StageError::new("validate", e.to_string()))?;
        for w in &report.warnings {
            log::warn!("{}: {w}", app.id);
        }
        let record = InstalledRelease {
            tag: c.tag.clone(),
            asset: c.asset.name.clone(),
            sha256: sha256.into(),
            installed_at: now_iso(),
            size: written,
        };
        layout::write_marker(&site, &record).map_err(io_stage("extract"))?;
        let added = precompress::precompress_tree(&site)
            .map_err(|e| StageError::new("extract", local_message(&e)))?;
        log::info!("{}: wrote {added} precompressed copies", app.id);
        let base = candidate::base_url(&self.cfg.paths.staging()).map_err(|e| {
            StageError::transient(
                "serve-check",
                format!("cannot start candidate listener: {e}"),
            )
        })?;
        let staged = staging.file_name().unwrap().to_string_lossy();
        validate::check_serving(
            &format!("{base}{staged}/site/"),
            &report,
            Duration::from_secs(self.cfg.updater.http_timeout_secs),
        )
        .map_err(|e| match e {
            ValidationError::Unavailable(m) => StageError::transient("serve-check", m),
            ValidationError::Invalid(m) => StageError::new("serve-check", m),
        })?;
        let dest = layout::promote(self.cfg, app, &site, version).map_err(io_stage("publish"))?;
        strip_write_bits(&dest).map_err(|e| StageError::new("publish", local_message(&e)))?;
        Ok(record)
    }

    /// Remove releases outside the retention policy. Active, pending and pinned releases are
    /// kept, as are releases that served requests recently (open tabs may still load from them).
    pub fn apply_retention(&self, app: &AppConfig) -> Result<Vec<String>> {
        let mut state = self.store.load(&app.id)?;
        let pin = self.effective_pin(app, &state);
        let horizon = now_epoch() - app.keep_days as i64 * 86_400;
        let recent = self.cfg.retention.keep_recently_used_secs;
        // `keep_latest` counts the active release first, then the most recently installed.
        let active = state.active.clone();
        let mut by_time: Vec<(&String, &InstalledRelease)> = state.installed.iter().collect();
        by_time.sort_by(|a, b| {
            let is_active = |v: &String| active.as_deref() == Some(v.as_str());
            is_active(b.0)
                .cmp(&is_active(a.0))
                .then_with(|| b.1.installed_at.cmp(&a.1.installed_at))
                .then_with(|| cmp_text(b.0, a.0))
        });
        let keep: Vec<String> = by_time
            .iter()
            .enumerate()
            .filter(|(i, (v, r))| {
                *i < app.keep_latest as usize
                    || (app.keep_days > 0
                        && parse_iso(&r.installed_at).is_some_and(|t| t >= horizon))
                    || state.active.as_deref() == Some(v.as_str())
                    || state.pending.as_deref() == Some(v.as_str())
                    || pin.as_deref() == Some(v.as_str())
                    || self
                        .activity
                        .as_ref()
                        .is_some_and(|a| a.release_used_within(&app.id, v, recent))
            })
            .map(|(_, (v, _))| (*v).clone())
            .collect();
        let remove: Vec<String> = state
            .installed
            .keys()
            .filter(|v| !keep.contains(v))
            .cloned()
            .collect();
        let mut removed = Vec::new();
        for v in remove {
            // A release published under another user id (CLI as a different member of the shared
            // group) may not be removable here; the server retries at its next check.
            let trashed = match layout::trash(self.cfg, app, &v) {
                Ok(t) => t,
                Err(e) => {
                    log::warn!("{}: retention cannot remove {v} yet: {e:#}", app.id);
                    continue;
                }
            };
            state.installed.remove(&v);
            self.store.save(&app.id, &state)?;
            if let Some(t) = trashed
                && let Err(e) = remove_tree(&t)
            {
                // Left in staging; startup reconciliation clears it.
                log::warn!("{}: delete {}: {e:#}", app.id, t.display());
            }
            self.store
                .record(&app.id, "retention", "removed", Some(&v), "")?;
            log::info!("{}: retention removed {v}", app.id);
            removed.push(v);
        }
        Ok(removed)
    }

    /// Delete least recently used archives until the cache fits `cache_max_size`.
    pub fn prune_cache(&self) -> Result<()> {
        let root = self.cfg.paths.cache.join("archives");
        let mut files = Vec::new();
        for app_dir in fs::read_dir(&root).into_iter().flatten().flatten() {
            for f in fs::read_dir(app_dir.path()).into_iter().flatten().flatten() {
                if let Ok(m) = f.metadata()
                    && m.is_file()
                {
                    files.push((m.modified().ok(), m.len(), f.path()));
                }
            }
        }
        files.sort_by_key(|a| a.0);
        let mut total: u64 = files.iter().map(|f| f.1).sum();
        for (_, len, path) in files {
            if total <= self.cfg.retention.cache_max_bytes {
                break;
            }
            fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
            total -= len;
            log::info!("cache: removed {}", path.display());
        }
        Ok(())
    }

    pub fn rollback(&self, app: &AppConfig, to: Option<&str>) -> Result<String> {
        let mut state = self.store.load(&app.id)?;
        let active = layout::active_version(self.cfg, app)
            .or(state.active.clone())
            .ok_or_else(|| anyhow!("{} has no active release", app.id))?;
        if self.effective_pin(app, &state).as_deref() == Some(active.as_str()) {
            bail!(
                "{} is pinned to {active}; pin the release to roll back to instead, or unpin first",
                app.id
            );
        }
        let on_disk: Vec<String> = layout::installed_dirs(self.cfg, app)
            .into_iter()
            .filter(|(_, m)| m.is_some())
            .map(|(v, _)| v)
            .collect();
        let target = match to {
            Some(t) => {
                let t = t.trim_start_matches('v').to_string();
                if t == active {
                    bail!("{t} is already active");
                }
                if !on_disk.contains(&t) {
                    bail!(
                        "{} {t} is not a retained release; retained: {}",
                        app.id,
                        on_disk.join(", ")
                    );
                }
                t
            }
            None => on_disk
                .iter()
                .filter(|v| cmp_text(v, &active).is_lt())
                .max_by(|a, b| cmp_text(a, b))
                .cloned()
                .ok_or_else(|| anyhow!("{} has no retained release older than {active}", app.id))?,
        };
        // Block first: if the process stops before the pointer switch, the old release stays
        // active (reconcile follows `current`) and nothing reinstalls it behind the operator.
        if !state.blocked.contains(&active) {
            state.blocked.push(active.clone());
        }
        state.pending = None;
        self.store.save(&app.id, &state)?;
        layout::activate(self.cfg, app, &target)?;
        state.active = Some(target.clone());
        state.last_update = Some(now_iso());
        self.store.save(&app.id, &state)?;
        self.store.record(
            &app.id,
            "rollback",
            "ok",
            Some(&target),
            &format!("from {active}; {active} blocked"),
        )?;
        Ok(format!(
            "{}: rolled back from {active} to {target}; {active} will not be reinstalled automatically until allowed",
            app.id
        ))
    }

    pub fn allow(&self, app: &AppConfig, version: &str) -> Result<String> {
        let v = version.trim_start_matches('v');
        let mut state = self.store.load(&app.id)?;
        if !state.blocked.iter().any(|b| b == v) {
            bail!(
                "{} {v} is not blocked (blocked: {})",
                app.id,
                if state.blocked.is_empty() {
                    "none".into()
                } else {
                    state.blocked.join(", ")
                }
            );
        }
        state.blocked.retain(|b| b != v);
        state.retry_after = None;
        self.store.save(&app.id, &state)?;
        self.store.record(&app.id, "allow", "ok", Some(v), "")?;
        Ok(format!(
            "{}: {v} may be installed again by the next update",
            app.id
        ))
    }

    pub fn pin(&self, app: &AppConfig, version: &str) -> Result<Outcome> {
        let parsed = parse_tag(version)
            .ok_or_else(|| anyhow!("{version:?} is not a release version such as 0.5.0"))?;
        let v = parsed.to_string();
        let mut state = self.store.load(&app.id)?;
        state.pinned = Some(v.clone());
        state.pending = None;
        state.blocked.retain(|b| *b != v);
        state.retry_after = None;
        self.store.save(&app.id, &state)?;
        self.store.record(&app.id, "pin", "ok", Some(&v), "")?;
        Ok(self.update_app(app, true))
    }

    pub fn unpin(&self, app: &AppConfig) -> Result<String> {
        let mut state = self.store.load(&app.id)?;
        let old = state.pinned.take();
        self.store.save(&app.id, &state)?;
        self.store
            .record(&app.id, "unpin", "ok", old.as_deref(), "")?;
        let mut msg = match old {
            Some(v) => format!("{}: unpinned from {v}", app.id),
            None => format!("{}: no CLI pin was set", app.id),
        };
        if let Some(p) = &app.pinned_version {
            msg.push_str(&format!(
                "; still pinned to {p} by pinned_version in config.toml"
            ));
        }
        Ok(msg)
    }
}

pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut f = fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(hex(&h.finalize()));
        }
        h.update(&buf[..n]);
    }
}

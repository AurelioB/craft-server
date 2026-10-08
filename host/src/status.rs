//! Status reporting: the public, non-sensitive `/status.json` read by the launcher, the admin
//! interface's detailed view, and the operator's `status` command.

use serde::Serialize;

use crate::config::{Activation, AppConfig, Config};
use crate::layout;
use crate::store::{AppState, Failure, Store};
use crate::timeutil::now_iso;
use crate::version::cmp_text;

const MAX_MESSAGE: usize = 240;

#[derive(Serialize)]
pub struct PublicError {
    pub at: String,
    pub stage: String,
    pub message: String,
}

#[derive(Serialize)]
pub struct AppStatus {
    pub id: String,
    pub name: String,
    /// Stable entry path, relative to the site root.
    pub url: String,
    /// "ready", "installing", "failed" or "disabled".
    pub state: String,
    pub enabled: bool,
    pub active: Option<String>,
    pub active_url: Option<String>,
    /// Logo URL relative to the site root: the official logo for built-in apps, otherwise the
    /// release's `icon`.
    pub icon: Option<String>,
    pub category: String,
    pub tagline: String,
    /// Brand color, `#rrggbb`, or empty.
    pub color: String,
    /// Installed release waiting for the app to be idle.
    pub pending: Option<String>,
    /// "immediate" or "idle".
    pub activation: &'static str,
    pub installed: Vec<String>,
    pub latest: Option<String>,
    pub update_available: bool,
    pub pinned: Option<String>,
    pub auto_update: bool,
    pub blocked: Vec<String>,
    pub last_check: Option<String>,
    pub last_check_ok: Option<bool>,
    pub last_update: Option<String>,
    pub error: Option<PublicError>,
    pub failed_versions: Vec<String>,
    pub notes: String,
}

#[derive(Serialize)]
pub struct UpdaterStatus {
    pub version: &'static str,
    pub heartbeat_at: Option<String>,
    pub phase: Option<String>,
    pub check_interval_seconds: u64,
}

#[derive(Serialize)]
pub struct Status {
    pub generated_at: String,
    pub updater: UpdaterStatus,
    pub apps: Vec<AppStatus>,
}

/// Replace container paths with directory names and bound the length: public summaries must
/// not disclose filesystem details.
pub fn sanitize(cfg: &Config, message: &str) -> String {
    let mut out = message.to_string();
    let mut roots = vec![
        (cfg.paths.data.to_string_lossy().into_owned(), "DATA_DIR"),
        (cfg.paths.state.to_string_lossy().into_owned(), "STATE_DIR"),
        (cfg.paths.cache.to_string_lossy().into_owned(), "CACHE_DIR"),
        (cfg.paths.work.to_string_lossy().into_owned(), "WORK_DIR"),
        (
            cfg.paths.config_dir().to_string_lossy().into_owned(),
            "CONFIG_DIR",
        ),
    ];
    roots.sort_by_key(|r| std::cmp::Reverse(r.0.len()));
    for (root, label) in roots {
        if root.len() > 1 {
            out = out.replace(&root, label);
        }
    }
    if out.chars().count() > MAX_MESSAGE {
        out = out.chars().take(MAX_MESSAGE - 1).collect::<String>() + "…";
    }
    out
}

fn public_error(cfg: &Config, f: &Failure) -> PublicError {
    PublicError {
        at: f.at.clone(),
        stage: f.stage.clone(),
        message: sanitize(cfg, &f.message),
    }
}

pub fn app_status(cfg: &Config, app: &AppConfig, state: &AppState, public: bool) -> AppStatus {
    let active = layout::active_version(cfg, app);
    let mut installed: Vec<String> = state.installed.keys().cloned().collect();
    installed.sort_by(|a, b| cmp_text(b, a));
    let latest = state.latest_seen.as_ref().map(|s| s.version.clone());
    let pinned = state.pinned.clone().or_else(|| app.pinned_version.clone());
    let update_available = match (&latest, &active) {
        (Some(l), Some(a)) => {
            cmp_text(l, a).is_gt()
                && !state.blocked.contains(l)
                && pinned.is_none()
                && state.pending.as_deref() != Some(l.as_str())
        }
        _ => false,
    };
    let error = state.last_error.as_ref().map(|f| {
        if public {
            public_error(cfg, f)
        } else {
            PublicError {
                at: f.at.clone(),
                stage: f.stage.clone(),
                message: f.message.clone(),
            }
        }
    });
    let st = if !app.enabled {
        "disabled"
    } else if active.is_some() {
        "ready"
    } else if error.is_some() {
        "failed"
    } else {
        "installing"
    };
    // Links are relative to the launcher, or absolute when the app has its own origin.
    let base = app
        .origin
        .as_ref()
        .map(|o| format!("{o}/"))
        .unwrap_or_default();
    AppStatus {
        id: app.id.clone(),
        name: app.name.clone(),
        url: format!("{base}{}/", app.entry),
        state: st.into(),
        enabled: app.enabled,
        active_url: active.as_ref().map(|v| format!("{base}{}/{v}/", app.entry)),
        icon: crate::assets::logo(&app.id).or_else(|| {
            active
                .as_ref()
                .filter(|v| {
                    !app.icon.is_empty()
                        && cfg
                            .release_root(app)
                            .join(v.as_str())
                            .join(&app.icon)
                            .is_file()
                })
                .map(|v| format!("{}/{v}/{}", app.entry, app.icon))
        }),
        category: app.category.clone(),
        tagline: app.tagline.clone(),
        color: app.color.clone(),
        active,
        pending: state.pending.clone(),
        activation: match app.activation {
            Activation::Immediate => "immediate",
            Activation::Idle => "idle",
        },
        installed,
        latest,
        update_available,
        pinned,
        auto_update: app.auto_update,
        blocked: state.blocked.clone(),
        last_check: state.last_check.clone(),
        last_check_ok: state.last_check_ok,
        last_update: state.last_update.clone(),
        error,
        failed_versions: state.failed_versions.keys().cloned().collect(),
        notes: app.notes.clone(),
    }
}

pub fn build(cfg: &Config, store: &Store, public: bool) -> Status {
    let hb = store.heartbeat();
    let apps = cfg
        .apps
        .iter()
        .filter(|a| a.enabled || !public)
        .map(|a| match store.load(&a.id) {
            Ok(state) => app_status(cfg, a, &state, public),
            Err(e) => {
                // Never pass a corrupt record off as "nothing installed".
                let mut s = app_status(cfg, a, &AppState::default(), public);
                let message = if public {
                    "state record unreadable; run `doctor`".to_string()
                } else {
                    format!("{e:#}")
                };
                s.error = Some(PublicError {
                    at: now_iso(),
                    stage: "state".into(),
                    message,
                });
                if s.state != "disabled" && s.state != "ready" {
                    s.state = "failed".into();
                }
                s
            }
        })
        .collect();
    Status {
        generated_at: now_iso(),
        updater: UpdaterStatus {
            version: env!("CARGO_PKG_VERSION"),
            heartbeat_at: hb.as_ref().map(|h| h.at.clone()),
            phase: hb.map(|h| h.phase),
            check_interval_seconds: cfg.updater.check_interval_secs,
        },
        apps,
    }
}

pub fn render_table(status: &Status) -> String {
    let mut rows = vec![
        [
            "APP",
            "STATE",
            "ACTIVE",
            "LATEST",
            "PENDING",
            "PINNED",
            "LAST CHECK",
            "PROBLEM",
        ]
        .map(String::from)
        .to_vec(),
    ];
    for a in &status.apps {
        let mut problem = a
            .error
            .as_ref()
            .map(|e| format!("{}: {}", e.stage, e.message))
            .unwrap_or_default();
        if !a.blocked.is_empty() {
            let b = format!("blocked {}", a.blocked.join(","));
            problem = if problem.is_empty() {
                b
            } else {
                format!("{b}; {problem}")
            };
        }
        if a.update_available && !a.auto_update {
            problem = if problem.is_empty() {
                "update available".into()
            } else {
                format!("update available; {problem}")
            };
        }
        rows.push(vec![
            a.id.clone(),
            a.state.clone(),
            a.active.clone().unwrap_or_else(|| "-".into()),
            a.latest.clone().unwrap_or_else(|| "-".into()),
            a.pending.clone().unwrap_or_else(|| "-".into()),
            a.pinned.clone().unwrap_or_else(|| "-".into()),
            a.last_check.clone().unwrap_or_else(|| "never".into()),
            problem,
        ]);
    }
    let widths: Vec<usize> = (0..rows[0].len())
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for r in rows {
        let line: Vec<String> = r
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c:<w$}", w = widths[i]))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out.push_str(&format!(
        "updater heartbeat: {} ({})\n",
        status.updater.heartbeat_at.as_deref().unwrap_or("none"),
        status.updater.phase.as_deref().unwrap_or("not running")
    ));
    out
}

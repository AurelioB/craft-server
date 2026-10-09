//! The long-running process: serves the apps and, in background threads, checks for updates on
//! a schedule, activates idle pending releases and writes a heartbeat. Serving never waits for
//! the updater; an updater failure only shows up in status.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use parking_lot::Mutex;

use crate::activity::Activity;
use crate::config::Config;
use crate::ops::{Outcome, Updater};
use crate::server::{self, Shared};
use crate::store::{AppState, Store};
use crate::timeutil::now_epoch;

/// Minimum delay between cycles after transient failures.
const MIN_RETRY_SECS: i64 = 30;
/// How often pending releases are re-evaluated for idle activation.
const ACTIVATION_TICK_SECS: i64 = 60;

/// Run one update cycle. Scheduled cycles (`manual = false`) only check apps that are due (see
/// [`due_at`]), so a retry for one app does not query GitHub again for all the others.
pub fn cycle(cfg: &Config, updater: &Updater, manual: bool) -> Vec<(String, Outcome)> {
    let mut results = Vec::new();
    let now = now_epoch();
    for app in cfg.enabled_apps() {
        if !manual
            && let Ok(st) = updater.store.load(&app.id)
            && due_at(cfg, &st, now) > now
        {
            continue;
        }
        let outcome = updater.update_app(app, manual);
        match &outcome {
            Outcome::Failed(_) => log::warn!("{}: {outcome}", app.id),
            _ => log::info!("{}: {outcome}", app.id),
        }
        results.push((app.id.clone(), outcome));
    }
    if let Err(e) = updater.prune_cache() {
        log::warn!("cache pruning failed: {e:#}");
    }
    results
}

/// When an app's next scheduled check is due: `check_interval` after its last successful check
/// (at once if it never had one or the last one failed), and never before a retry deadline set
/// after failures (backoff, or GitHub's rate-limit reset).
pub fn due_at(cfg: &Config, st: &AppState, now: i64) -> i64 {
    let base = match (st.last_check_ok, st.last_check.as_deref()) {
        (Some(true), Some(at)) => crate::timeutil::parse_iso(at)
            .map_or(now, |t| t + cfg.updater.check_interval_secs as i64),
        _ => now,
    };
    base.max(st.retry_after.unwrap_or(i64::MIN))
}

fn earliest_due(cfg: &Config, store: &Store, now: i64) -> i64 {
    cfg.enabled_apps()
        .map(|app| match store.load(&app.id) {
            Ok(st) => due_at(cfg, &st, now),
            Err(_) => now,
        })
        .min()
        .unwrap_or(now + cfg.updater.check_interval_secs as i64)
}

fn next_run(cfg: &Config, store: &Store) -> i64 {
    let now = now_epoch();
    earliest_due(cfg, store, now).max(now + MIN_RETRY_SECS)
}

/// When the first scheduled check runs after a start. Restarts must not cost API requests:
/// without a token every request counts against GitHub's 60 per hour, conditional ones too. With
/// `check_on_startup` the schedule simply continues, so only apps that are due are checked at
/// once; without it the first check waits a full `check_interval`.
pub fn first_run(cfg: &Config, store: &Store) -> i64 {
    let now = now_epoch();
    if !cfg.updater.check_on_startup {
        return now + cfg.updater.check_interval_secs as i64;
    }
    earliest_due(cfg, store, now).max(now)
}

/// Activate pending releases whose apps have been idle long enough. Never waits for the lock.
pub fn activate_idle(cfg: &Config, updater: &Updater) {
    let pending: Vec<_> = cfg
        .enabled_apps()
        .filter(|a| updater.store.load(&a.id).is_ok_and(|s| s.pending.is_some()))
        .collect();
    if pending.is_empty() {
        return;
    }
    let Ok(_lock) = updater.store.lock(Duration::ZERO, "idle activation") else {
        return;
    };
    for app in pending {
        match updater.apply_pending(app, false) {
            Ok(Some(v)) => log::info!("{}: activated {v} while idle", app.id),
            Ok(None) => {}
            Err(e) => log::warn!("{}: idle activation failed: {e:#}", app.id),
        }
    }
}

fn updater_loop(cfg: Arc<Config>, activity: Arc<Activity>, phase: Arc<Mutex<String>>) {
    let updater = Updater::new(&cfg).with_activity(activity);
    let lock_wait = Duration::from_secs(cfg.updater.lock_wait_secs);
    // Releases installed by earlier versions lack precompressed copies; add them in the
    // background so serving starts at once.
    match updater.store.lock(lock_wait, "precompress backfill") {
        Ok(_lock) => {
            if let Err(e) = crate::layout::backfill_precompressed(&cfg) {
                log::warn!("precompress backfill: {e:#}");
            }
        }
        Err(e) => log::warn!("{e:#}"),
    }
    let mut next = first_run(&cfg, &updater.store);
    if next > now_epoch() {
        log::info!(
            "last check is recent; next check at {}",
            crate::timeutil::iso(next)
        );
    }
    let mut next_tick = now_epoch() + ACTIVATION_TICK_SECS;
    loop {
        let now = now_epoch();
        if now >= next_tick {
            activate_idle(&cfg, &updater);
            next_tick = now + ACTIVATION_TICK_SECS;
        }
        if now < next {
            std::thread::sleep(Duration::from_secs(
                (next.min(next_tick) - now).clamp(1, 60) as u64,
            ));
            continue;
        }
        *phase.lock() = "updating".into();
        match updater.store.lock(lock_wait, "scheduled update") {
            Ok(_lock) => match updater.prepare() {
                Ok(()) => {
                    cycle(&cfg, &updater, false);
                }
                Err(e) => log::error!("update cycle failed: {e:#}"),
            },
            Err(e) => log::warn!("{e:#}"),
        }
        *phase.lock() = "idle".into();
        next = next_run(&cfg, &updater.store);
        log::info!("next check at {}", crate::timeutil::iso(next));
    }
}

pub fn run(cfg: Config) -> Result<()> {
    let cfg = Arc::new(cfg);
    let activity = Arc::new(Activity::new());
    {
        let updater = Updater::new(&cfg);
        let _lock = updater
            .store
            .lock(Duration::from_secs(cfg.updater.lock_wait_secs), "startup")?;
        updater.prepare()?;
        crate::layout::reconcile(&cfg, &updater.store)?;
        updater.store.beat("starting")?;
    }
    if cfg.admin.enabled && cfg.auth.sign_in == crate::access::SignIn::None {
        log::warn!("admin interface is enabled without sign-in ([auth] methods is empty)");
    }

    let phase = Arc::new(Mutex::new(String::from("idle")));
    {
        let (cfg, phase) = (cfg.clone(), phase.clone());
        std::thread::Builder::new()
            .name("heartbeat".into())
            .spawn(move || {
                let store = Store::new(&cfg.paths.state);
                loop {
                    let p = phase.lock().clone();
                    if let Err(e) = store.beat(&p) {
                        log::warn!("heartbeat: {e:#}");
                    }
                    std::thread::sleep(Duration::from_secs(cfg.updater.heartbeat_secs));
                }
            })?;
    }
    {
        let (cfg, activity, phase) = (cfg.clone(), activity.clone(), phase.clone());
        std::thread::Builder::new()
            .name("updater".into())
            .spawn(move || updater_loop(cfg, activity, phase))?;
    }

    let shared = Shared::new(cfg, activity)?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(server::serve(shared))
}

/// Container health check: the HTTP listener answers `/healthz`. The updater's heartbeat is
/// reported in status instead, so an updater problem never restarts a working server.
pub fn healthy(cfg: &Config) -> Result<(), String> {
    let mut addr = cfg.server.listen;
    if addr.ip().is_unspecified() {
        let loopback: std::net::IpAddr = if addr.is_ipv6() {
            std::net::Ipv6Addr::LOCALHOST.into()
        } else {
            std::net::Ipv4Addr::LOCALHOST.into()
        };
        addr.set_ip(loopback);
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .new_agent();
    match agent.get(&format!("http://{addr}/healthz")).call() {
        Ok(r) if r.status().as_u16() == 200 => Ok(()),
        Ok(r) => Err(format!("/healthz answered HTTP {}", r.status().as_u16())),
        Err(e) => Err(format!("/healthz unreachable: {e}")),
    }
}

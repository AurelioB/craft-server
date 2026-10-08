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
use crate::store::Store;
use crate::timeutil::now_epoch;

/// Minimum delay between cycles after transient failures.
const MIN_RETRY_SECS: i64 = 30;
/// How often pending releases are re-evaluated for idle activation.
const ACTIVATION_TICK_SECS: i64 = 60;

/// Run one update cycle over all enabled apps.
pub fn cycle(cfg: &Config, updater: &Updater, manual: bool) -> Vec<(String, Outcome)> {
    let mut results = Vec::new();
    for app in cfg.enabled_apps() {
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

fn next_run(cfg: &Config, store: &Store) -> i64 {
    let now = now_epoch();
    let mut next = now + cfg.updater.check_interval_secs as i64;
    for app in cfg.enabled_apps() {
        if let Ok(st) = store.load(&app.id)
            && let Some(at) = st.retry_after
        {
            next = next.min(at.max(now + MIN_RETRY_SECS));
        }
    }
    next
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
    let mut next = if cfg.updater.check_on_startup {
        now_epoch()
    } else {
        now_epoch() + cfg.updater.check_interval_secs as i64
    };
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
    if cfg.admin.enabled && cfg.auth.method == crate::access::AuthMethod::None {
        log::warn!("admin interface is enabled without sign-in ([auth] method = \"none\")");
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

    let shared = Shared::new(cfg, activity);
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

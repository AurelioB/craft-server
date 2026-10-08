//! The long-running updater: reconcile, publish status, then check and update on a schedule.
//! Installed releases are served by the web server whether or not this process runs.

use std::sync::Arc;

use parking_lot::Mutex;
use std::time::Duration;

use anyhow::Result;

use crate::config::Config;
use crate::ops::{Outcome, Updater};
use crate::status;
use crate::store::Store;
use crate::timeutil::now_epoch;

/// Minimum delay between cycles after transient failures.
const MIN_RETRY_SECS: i64 = 30;

/// Run one update cycle over all enabled apps. Returns the epoch of the next cycle.
pub fn cycle(cfg: &Config, updater: &Updater, manual: bool) -> Result<Vec<(String, Outcome)>> {
    let mut results = Vec::new();
    for app in cfg.enabled_apps() {
        let outcome = updater.update_app(app, manual);
        match &outcome {
            Outcome::Failed(_) => log::warn!("{}: {outcome}", app.id),
            _ => log::info!("{}: {outcome}", app.id),
        }
        results.push((app.id.clone(), outcome));
        status::publish(cfg, &updater.store)?;
    }
    if let Err(e) = updater.prune_cache() {
        log::warn!("cache pruning failed: {e:#}");
    }
    Ok(results)
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

pub fn run(cfg: &Config) -> Result<()> {
    let updater = Updater::new(cfg);
    let lock_wait = Duration::from_secs(cfg.updater.lock_wait_secs);
    {
        let _lock = updater.store.lock(lock_wait, "daemon startup")?;
        updater.prepare()?;
        crate::layout::reconcile(cfg, &updater.store)?;
        updater.store.beat("starting")?;
        status::publish(cfg, &updater.store)?;
    }

    let phase = Arc::new(Mutex::new(String::from("idle")));
    {
        let (cfg, phase) = (cfg.clone(), phase.clone());
        std::thread::spawn(move || {
            let store = Store::new(&cfg.paths.state);
            loop {
                let p = phase.lock().clone();
                if let Err(e) = store.beat(&p).and_then(|_| status::publish(&cfg, &store)) {
                    log::warn!("heartbeat: {e:#}");
                }
                std::thread::sleep(Duration::from_secs(cfg.updater.heartbeat_secs));
            }
        });
    }

    let mut next = if cfg.updater.check_on_startup {
        now_epoch()
    } else {
        now_epoch() + cfg.updater.check_interval_secs as i64
    };
    loop {
        let wait = next - now_epoch();
        if wait > 0 {
            std::thread::sleep(Duration::from_secs(wait.min(60) as u64));
            continue;
        }
        *phase.lock() = "updating".into();
        match updater.store.lock(lock_wait, "scheduled update") {
            Ok(_lock) => {
                if let Err(e) = updater
                    .prepare()
                    .and_then(|_| cycle(cfg, &updater, false).map(|_| ()))
                {
                    log::error!("update cycle failed: {e:#}");
                }
            }
            Err(e) => log::warn!("{e:#}"),
        }
        *phase.lock() = "idle".into();
        let _ = status::publish(cfg, &updater.store);
        next = next_run(cfg, &updater.store);
        log::info!("next check at {}", crate::timeutil::iso(next));
    }
}

/// Exit status for the container health check: the heartbeat must be recent.
pub fn healthy(cfg: &Config) -> Result<(), String> {
    let store = Store::new(&cfg.paths.state);
    let Some(hb) = store.heartbeat() else {
        return Err("no heartbeat yet".into());
    };
    let age = now_epoch() - hb.epoch;
    let max = (cfg.updater.heartbeat_secs * 4).max(120) as i64;
    if age > max {
        return Err(format!("heartbeat is {age}s old (limit {max}s)"));
    }
    Ok(())
}

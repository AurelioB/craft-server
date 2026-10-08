use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

use craft_updater::config::{Config, Paths, load_config};
use craft_updater::ops::{Outcome, Updater};
use craft_updater::{daemon, doctor, fsutil, layout, logging, status};

/// Craft Apps Host updater and operator CLI.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run continuously: reconcile, then check and update on the configured schedule.
    Daemon,
    /// Show installed, active and latest versions, pins and failures.
    Status {
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Check configuration, identities, permissions, space and connectivity.
    Doctor {
        /// Skip GitHub and validation-listener connectivity checks.
        #[arg(long)]
        offline: bool,
    },
    /// Look for newer eligible releases without installing them.
    Check { app: Option<String> },
    /// Install the newest eligible release (or the pinned one) now.
    Update { app: Option<String> },
    /// Keep an app on one release; installs it if needed.
    Pin { app: String, version: String },
    /// Remove a pin set with `pin`.
    Unpin { app: String },
    /// Activate a retained older release and block the current one from automatic reinstall.
    Rollback {
        app: String,
        version: Option<String>,
    },
    /// Allow a release blocked by a rollback to be installed again.
    Allow { app: String, version: String },
    /// Show recent update history.
    History {
        app: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Exit 0 if the daemon heartbeat is recent (container health check).
    Healthcheck,
}

fn load() -> Result<Config, String> {
    load_config(Paths::from_env()).map_err(|e| e.to_string())
}

fn apps<'c>(
    cfg: &'c Config,
    app: &Option<String>,
) -> Result<Vec<&'c craft_updater::config::AppConfig>, String> {
    match app {
        Some(id) => {
            let a = cfg.app(id).map_err(|e| e.to_string())?;
            if !a.enabled {
                return Err(format!("{id} is disabled in config.toml"));
            }
            Ok(vec![a])
        }
        None => Ok(cfg.enabled_apps().collect()),
    }
}

fn run(cli: Cli) -> Result<ExitCode, String> {
    let mutating = !matches!(
        cli.command,
        Command::Status { .. }
            | Command::Doctor { .. }
            | Command::History { .. }
            | Command::Healthcheck
    );
    // SAFETY: plain libc identity query.
    if mutating
        && unsafe { libc::geteuid() } == 0
        && std::env::var("CRAFT_ALLOW_ROOT").as_deref() != Ok("1")
    {
        return Err("refusing to run as root; set RUN_UID/RUN_GID to a non-root identity (or CRAFT_ALLOW_ROOT=1)".into());
    }
    if let Some(mask) = std::env::var("FILE_UMASK")
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        let mask = fsutil::parse_umask(&mask).map_err(|e| e.to_string())?;
        fsutil::set_umask(mask);
    }

    if let Command::Doctor { offline } = cli.command {
        let report = doctor::run(Paths::from_env(), !offline);
        print!("{}", report.render());
        return Ok(if report.worst() == doctor::Level::Fail {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        });
    }

    let cfg = load()?;
    logging::init(
        cfg.paths.logs.as_ref(),
        cfg.updater.log_max_bytes,
        cfg.updater.log_backups,
    );
    let updater = Updater::new(&cfg);
    let lock_wait = Duration::from_secs(cfg.updater.lock_wait_secs);
    let locked = |what: &str| {
        updater
            .store
            .lock(lock_wait, what)
            .map_err(|e| format!("{e:#}"))
    };
    let err = |e: anyhow::Error| format!("{e:#}");

    match cli.command {
        Command::Daemon => daemon::run(&cfg).map_err(err).map(|_| ExitCode::SUCCESS),
        Command::Healthcheck => match daemon::healthy(&cfg) {
            Ok(()) => Ok(ExitCode::SUCCESS),
            Err(e) => {
                eprintln!("unhealthy: {e}");
                Ok(ExitCode::FAILURE)
            }
        },
        Command::Status { json } => {
            let s = status::build(&cfg, &updater.store, false);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&s).map_err(|e| e.to_string())?
                );
            } else {
                print!("{}", status::render_table(&s));
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::History { app, limit } => {
            for e in updater.store.history(app.as_deref(), limit) {
                let v = e.version.as_deref().unwrap_or("-");
                println!(
                    "{}  {:<12} {:<9} {:<8} {v:<12} {}",
                    e.at, e.app, e.action, e.outcome, e.message
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Check { app } => {
            let targets = apps(&cfg, &app)?;
            let _lock = locked("check")?;
            updater.prepare().map_err(err)?;
            let mut failed = false;
            for a in targets {
                match updater.check(a) {
                    Ok(r) => println!(
                        "{}: active {}, latest {}{}",
                        a.id,
                        r.active.as_deref().unwrap_or("none"),
                        r.latest.as_deref().unwrap_or("none"),
                        if r.update_available {
                            " (update available)"
                        } else {
                            ""
                        }
                    ),
                    Err(e) => {
                        failed = true;
                        println!("{}: check failed: {e:#}", a.id);
                    }
                }
            }
            status::publish(&cfg, &updater.store).map_err(err)?;
            Ok(if failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            })
        }
        Command::Update { app } => {
            let targets = apps(&cfg, &app)?;
            let _lock = locked("manual update")?;
            updater.prepare().map_err(err)?;
            layout::reconcile(&cfg, &updater.store).map_err(err)?;
            let mut failed = false;
            for a in targets {
                let outcome = updater.update_app(a, true);
                failed |= matches!(outcome, Outcome::Failed(_));
                println!("{}: {outcome}", a.id);
                status::publish(&cfg, &updater.store).map_err(err)?;
            }
            updater.prune_cache().map_err(err)?;
            Ok(if failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            })
        }
        Command::Pin { app, version } => {
            let a = cfg.app(&app).map_err(|e| e.to_string())?;
            let _lock = locked("pin")?;
            updater.prepare().map_err(err)?;
            let outcome = updater.pin(a, &version).map_err(err)?;
            status::publish(&cfg, &updater.store).map_err(err)?;
            println!(
                "{app}: pinned to {}; {outcome}",
                version.trim_start_matches('v')
            );
            Ok(if matches!(outcome, Outcome::Failed(_)) {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            })
        }
        Command::Unpin { app } => {
            let a = cfg.app(&app).map_err(|e| e.to_string())?;
            let _lock = locked("unpin")?;
            println!("{}", updater.unpin(a).map_err(err)?);
            status::publish(&cfg, &updater.store).map_err(err)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Rollback { app, version } => {
            let a = cfg.app(&app).map_err(|e| e.to_string())?;
            let _lock = locked("rollback")?;
            println!("{}", updater.rollback(a, version.as_deref()).map_err(err)?);
            status::publish(&cfg, &updater.store).map_err(err)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Allow { app, version } => {
            let a = cfg.app(&app).map_err(|e| e.to_string())?;
            let _lock = locked("allow")?;
            println!("{}", updater.allow(a, &version).map_err(err)?);
            status::publish(&cfg, &updater.store).map_err(err)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Doctor { .. } => unreachable!(),
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

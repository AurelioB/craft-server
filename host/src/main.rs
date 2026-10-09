use std::io::Read;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

use craft_host::access::Role;
use craft_host::config::{Config, Paths, load_config};
use craft_host::ops::{Outcome, Updater};
use craft_host::users::{NewUser, UserDb, UserUpdate};
use craft_host::{daemon, doctor, fsutil, layout, logging, status};

/// Craft Apps Host: serves the Craft browser apps, keeps them updated, and the operator CLI.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the apps and check for updates on the configured schedule.
    Serve,
    /// Show installed, active and latest versions, pins and failures.
    Status {
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Check configuration, identities, permissions, space and connectivity.
    Doctor {
        /// Skip GitHub connectivity checks.
        #[arg(long)]
        offline: bool,
    },
    /// Look for newer eligible releases without installing them.
    Check { app: Option<String> },
    /// Install the newest eligible release (or the pinned one) now.
    Update { app: Option<String> },
    /// Activate a release that is waiting for its app to be idle, now.
    Apply { app: String },
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
    /// Exit 0 if the server answers /healthz (container health check).
    Healthcheck,
    /// Manage accounts in the user database (STATE_DIR/users.sqlite3). Passwords are read
    /// from standard input.
    User {
        #[command(subcommand)]
        action: UserCommand,
    },
}

#[derive(Subcommand)]
enum UserCommand {
    /// List accounts.
    List,
    /// Add an account; the password is read from standard input unless --no-password.
    Add {
        name: String,
        #[arg(long)]
        email: Option<String>,
        /// user or admin
        #[arg(long, default_value = "user")]
        role: String,
        /// Create the account without a password (OIDC sign-in only).
        #[arg(long)]
        no_password: bool,
    },
    /// Set a new password (from standard input); ends the account's sessions.
    SetPassword { name: String },
    /// Set the role: user or admin.
    SetRole { name: String, role: String },
    /// Set the e-mail address; without one, remove it.
    SetEmail { name: String, email: Option<String> },
    /// Remove the linked OpenID Connect identity.
    Unlink { name: String },
    /// Delete an account.
    Delete { name: String },
    /// Import "name:<argon2 hash>[:user|admin]" lines, e.g. from an earlier users file.
    Import { file: std::path::PathBuf },
}

fn load() -> Result<Config, String> {
    load_config(Paths::from_env()).map_err(|e| e.to_string())
}

fn apps<'c>(
    cfg: &'c Config,
    app: &Option<String>,
) -> Result<Vec<&'c craft_host::config::AppConfig>, String> {
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
            | Command::User {
                action: UserCommand::List
            }
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
        Command::Serve => daemon::run(cfg.clone())
            .map_err(err)
            .map(|_| ExitCode::SUCCESS),
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
            }
            updater.prune_cache().map_err(err)?;
            Ok(if failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            })
        }
        Command::Apply { app } => {
            let a = cfg.app(&app).map_err(|e| e.to_string())?;
            let _lock = locked("apply")?;
            match updater.apply_pending(a, true).map_err(err)? {
                Some(v) => println!("{app}: activated {v}"),
                None => println!("{app}: no release is waiting for activation"),
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Pin { app, version } => {
            let a = cfg.app(&app).map_err(|e| e.to_string())?;
            let _lock = locked("pin")?;
            updater.prepare().map_err(err)?;
            let outcome = updater.pin(a, &version).map_err(err)?;
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
            Ok(ExitCode::SUCCESS)
        }
        Command::Rollback { app, version } => {
            let a = cfg.app(&app).map_err(|e| e.to_string())?;
            let _lock = locked("rollback")?;
            println!("{}", updater.rollback(a, version.as_deref()).map_err(err)?);
            Ok(ExitCode::SUCCESS)
        }
        Command::Allow { app, version } => {
            let a = cfg.app(&app).map_err(|e| e.to_string())?;
            let _lock = locked("allow")?;
            println!("{}", updater.allow(a, &version).map_err(err)?);
            Ok(ExitCode::SUCCESS)
        }
        Command::User { action } => user_command(&cfg, action),
        Command::Doctor { .. } => unreachable!(),
    }
}

fn read_password() -> Result<String, String> {
    let mut pw = String::new();
    std::io::stdin()
        .read_to_string(&mut pw)
        .map_err(|e| e.to_string())?;
    let pw = pw.trim_end_matches(['\r', '\n']).to_string();
    if pw.is_empty() {
        return Err("no password on standard input".into());
    }
    Ok(pw)
}

fn parse_role(text: &str) -> Result<Role, String> {
    Role::parse(text).ok_or_else(|| format!("{text:?}: the role is user or admin"))
}

fn user_command(cfg: &Config, action: UserCommand) -> Result<ExitCode, String> {
    let db = UserDb::open(&cfg.paths.state).map_err(|e| format!("{e:#}"))?;
    let find = |name: &str| -> Result<i64, String> {
        db.by_username(name)
            .map_err(|e| e.to_string())?
            .map(|u| u.id)
            .ok_or_else(|| format!("no user {name:?}"))
    };
    let e = |e: craft_host::users::UserError| e.to_string();
    match action {
        UserCommand::List => {
            println!(
                "{:<24} {:<6} {:<30} {:<8} {:<5} LAST SIGN-IN",
                "USER", "ROLE", "E-MAIL", "PASSWORD", "OIDC"
            );
            for u in db.list().map_err(e)? {
                println!(
                    "{:<24} {:<6} {:<30} {:<8} {:<5} {}",
                    u.username,
                    u.role.name(),
                    u.email.as_deref().unwrap_or("-"),
                    if u.has_password { "yes" } else { "no" },
                    if u.oidc.is_some() { "yes" } else { "no" },
                    u.last_login_at.as_deref().unwrap_or("never")
                );
            }
        }
        UserCommand::Add {
            name,
            email,
            role,
            no_password,
        } => {
            let role = parse_role(&role)?;
            let password = if no_password {
                None
            } else {
                Some(read_password()?)
            };
            let u = db
                .create(NewUser {
                    username: &name,
                    email: email.as_deref(),
                    password: password.as_deref(),
                    role,
                })
                .map_err(e)?;
            println!("added {} ({})", u.username, u.role.name());
        }
        UserCommand::SetPassword { name } => {
            let id = find(&name)?;
            let pw = read_password()?;
            db.update(
                id,
                UserUpdate {
                    password: Some(&pw),
                    ..Default::default()
                },
            )
            .map_err(e)?;
            println!("{name}: password changed; existing sessions end");
        }
        UserCommand::SetRole { name, role } => {
            let id = find(&name)?;
            let u = db
                .update(
                    id,
                    UserUpdate {
                        role: Some(parse_role(&role)?),
                        ..Default::default()
                    },
                )
                .map_err(e)?;
            println!("{}: role {}", u.username, u.role.name());
        }
        UserCommand::SetEmail { name, email } => {
            let id = find(&name)?;
            let u = db
                .update(
                    id,
                    UserUpdate {
                        email: Some(email.as_deref()),
                        ..Default::default()
                    },
                )
                .map_err(e)?;
            println!(
                "{}: e-mail {}",
                u.username,
                u.email.as_deref().unwrap_or("removed")
            );
        }
        UserCommand::Unlink { name } => {
            db.unlink_oidc(find(&name)?).map_err(e)?;
            println!("{name}: linked identity removed; existing sessions end");
        }
        UserCommand::Delete { name } => {
            db.delete(find(&name)?).map_err(e)?;
            println!("{name}: deleted");
        }
        UserCommand::Import { file } => {
            let text = std::fs::read_to_string(&file)
                .map_err(|err| format!("{}: {err}", file.display()))?;
            let mut failed = false;
            for (n, line) in text.lines().enumerate().map(|(i, l)| (i + 1, l.trim())) {
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let mut f = line.splitn(3, ':');
                let (Some(name), Some(hash)) = (f.next(), f.next()) else {
                    eprintln!("line {n}: expected name:<hash>[:role]");
                    failed = true;
                    continue;
                };
                let role = match f.next().map(str::trim).filter(|r| !r.is_empty()) {
                    None => Role::User,
                    Some(r) => match parse_role(r) {
                        Ok(r) => r,
                        Err(err) => {
                            eprintln!("line {n}: {err}");
                            failed = true;
                            continue;
                        }
                    },
                };
                match db.create_with_hash(name.trim(), hash.trim(), role) {
                    Ok(u) => println!("imported {} ({})", u.username, u.role.name()),
                    Err(err) => {
                        eprintln!("line {n}: {err}");
                        failed = true;
                    }
                }
            }
            if failed {
                return Ok(ExitCode::FAILURE);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
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

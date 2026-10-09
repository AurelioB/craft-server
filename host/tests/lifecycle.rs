//! End-to-end update lifecycle against a fake GitHub: install, update, failures that must not
//! disturb the working release, pins, rollback, retention, recovery and locking.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use common::*;
use craft_host::layout;
use craft_host::ops::{Outcome, Updater};
use craft_host::status;

const REPO: &str = "storytold/testcraft";

fn update(e: &Env) -> Outcome {
    let u = Updater::new(&e.cfg);
    u.prepare().unwrap();
    let app = e.cfg.app("testcraft").unwrap();
    u.update_app(app, true)
}

fn assert_failed(o: &Outcome, stage: &str, text: &str) {
    match o {
        Outcome::Failed(err) => {
            assert_eq!(err.stage, stage, "{o}");
            assert!(err.message.contains(text), "{o}");
        }
        other => panic!("expected failure in {stage}, got {other}"),
    }
}

/// (kind, version or reason) of an outcome, for compact assertions.
fn kind(o: &Outcome) -> (&'static str, String) {
    match o {
        Outcome::Installed(v) => ("installed", v.clone()),
        Outcome::Activated(v) => ("activated", v.clone()),
        Outcome::Staged(v) => ("staged", v.clone()),
        Outcome::UpToDate(v) => ("up-to-date", v.clone().unwrap_or_default()),
        Outcome::Skipped(why) => ("skipped", why.clone()),
        Outcome::Failed(e) => ("failed", e.message.clone()),
    }
}

fn installed(v: &str) -> (&'static str, String) {
    ("installed", v.to_string())
}

#[test]
fn installs_publishes_and_updates_atomically() {
    let gh = FakeGitHub::start();
    let e = env(&gh);
    gh.publish(
        REPO,
        "v1.0.0",
        release_zip(&e.root(), "1.0.0", &[]),
        PublishOpts::default(),
    );

    assert_eq!(kind(&update(&e)), installed("1.0.0"));
    assert_eq!(e.current().as_deref(), Some("1.0.0"));
    assert_eq!(e.served_version().as_deref(), Some("1.0.0"));

    // Published releases are read-only and carry no executable bits.
    let index = std::fs::metadata(e.release_dir("1.0.0").join("index.html")).unwrap();
    assert_eq!(index.permissions().mode() & 0o333, 0);
    assert!(e.release_dir("1.0.0").join(layout::MARKER).is_file());

    // Nothing new: no download, status ready.
    assert_eq!(kind(&update(&e)), ("up-to-date", "1.0.0".into()));
    let u = Updater::new(&e.cfg);
    let s = serde_json::to_value(status::build(&e.cfg, &u.store, true)).unwrap();
    let app = &s["apps"][0];
    assert_eq!(app["id"], "testcraft");
    assert_eq!(app["state"], "ready");
    assert_eq!(app["active_url"], "testcraft/1.0.0/");
    assert_eq!(
        s["apps"].as_array().unwrap().len(),
        1,
        "disabled apps are not published"
    );

    gh.publish(
        REPO,
        "v1.1.0",
        release_zip(&e.root(), "1.1.0", &[]),
        PublishOpts::default(),
    );
    assert_eq!(kind(&update(&e)), installed("1.1.0"));
    assert_eq!(e.served_version().as_deref(), Some("1.1.0"));
    assert!(
        !e.release_dir("1.0.0").exists(),
        "by default only the active release is kept (no open tab used 1.0.0)"
    );
    assert_eq!(
        u.store
            .load("testcraft")
            .unwrap()
            .installed
            .keys()
            .collect::<Vec<_>>(),
        ["1.1.0"]
    );
    assert_eq!(
        std::fs::read_dir(e.cfg.paths.staging()).unwrap().count(),
        0,
        "staging is empty after success"
    );
}

#[test]
fn integrity_failures_keep_the_working_release() {
    let gh = FakeGitHub::start();
    let e = env(&gh);
    gh.publish(
        REPO,
        "v1.0.0",
        release_zip(&e.root(), "1.0.0", &[]),
        PublishOpts::default(),
    );
    update(&e);

    // Published checksum does not match the bytes.
    let bad = "0".repeat(64);
    gh.publish(
        REPO,
        "v1.1.0",
        release_zip(&e.root(), "1.1.0", &[]),
        PublishOpts {
            sums_override: Some(bad.clone()),
            digest_override: Some(bad),
            ..Default::default()
        },
    );
    assert_failed(&update(&e), "checksum", "SHA-256 mismatch");
    assert_eq!(e.served_version().as_deref(), Some("1.0.0"));

    // SHA256SUMS and the API digest disagree.
    gh.publish(
        REPO,
        "v1.1.0",
        release_zip(&e.root(), "1.1.0", &[]),
        PublishOpts {
            digest_override: Some("f".repeat(64)),
            ..Default::default()
        },
    );
    assert_failed(&update(&e), "checksum", "disagree");

    // No integrity metadata at all: skipped.
    gh.publish(
        REPO,
        "v1.1.0",
        release_zip(&e.root(), "1.1.0", &[]),
        PublishOpts {
            sums: false,
            api_digest: false,
            ..Default::default()
        },
    );
    assert_failed(&update(&e), "checksum", "no published SHA-256");

    // Either source alone is enough.
    gh.publish(
        REPO,
        "v1.1.0",
        release_zip(&e.root(), "1.1.0", &[]),
        PublishOpts {
            sums: false,
            ..Default::default()
        },
    );
    assert!(matches!(update(&e), Outcome::Installed(_)));

    let st = Updater::new(&e.cfg).store.load("testcraft").unwrap();
    assert!(
        st.failed_versions.is_empty(),
        "a later success clears the failure record"
    );
    assert!(st.last_error.is_none());
}

#[test]
fn unsafe_and_incompatible_archives_are_rejected_and_reported() {
    let gh = FakeGitHub::start();
    let e = env(&gh);
    gh.publish(
        REPO,
        "v1.0.0",
        release_zip(&e.root(), "1.0.0", &[]),
        PublishOpts::default(),
    );
    update(&e);

    let evil = e.root().join("evil.zip");
    craft_host::testutil::raw_zip(
        &evil,
        &[
            ("x/index.html", None, b"<html>"),
            ("x/../../escape.txt", None, b"pwned"),
        ],
    );
    gh.publish(
        REPO,
        "v2.0.0",
        std::fs::read(&evil).unwrap(),
        PublishOpts::default(),
    );
    assert_failed(&update(&e), "archive", "path traversal");
    assert!(
        !e.cfg
            .paths
            .data
            .parent()
            .unwrap()
            .join("escape.txt")
            .exists()
    );

    let native = release_zip(
        &e.root(),
        "2.0.1",
        &[("build/script", b"\x7fELF\x02\x01\x01")],
    );
    gh.publish(REPO, "v2.0.1", native, PublishOpts::default());
    assert_failed(&update(&e), "validate", "native executable");

    let absolute = release_zip(
        &e.root(),
        "2.0.2",
        &[("sw.js", b"importScripts('/root-absolute.js')")],
    );
    gh.publish(REPO, "v2.0.2", absolute, PublishOpts::default());
    assert_failed(&update(&e), "validate", "root-absolute");

    assert_eq!(e.served_version().as_deref(), Some("1.0.0"));
    assert_eq!(
        std::fs::read_dir(e.cfg.paths.staging()).unwrap().count(),
        0,
        "failed staging is cleaned up"
    );
    let u = Updater::new(&e.cfg);
    let s = status::build(&e.cfg, &u.store, true);
    let app = &s.apps[0];
    assert_eq!(app.state, "ready");
    assert!(
        app.error
            .as_ref()
            .unwrap()
            .message
            .contains("root-absolute")
    );
    assert_eq!(app.failed_versions, ["2.0.0", "2.0.1", "2.0.2"]);
    assert!(
        !serde_json::to_string(&s)
            .unwrap()
            .contains(&e.cfg.paths.data.display().to_string()),
        "public status hides paths"
    );
}

#[test]
fn interrupted_downloads_offline_and_rate_limits_are_survivable() {
    let gh = FakeGitHub::start();
    let e = env(&gh);
    gh.publish(
        REPO,
        "v1.0.0",
        release_zip(&e.root(), "1.0.0", &[]),
        PublishOpts::default(),
    );
    update(&e);

    gh.publish(
        REPO,
        "v1.1.0",
        release_zip(&e.root(), "1.1.0", &[]),
        PublishOpts::default(),
    );
    gh.behave(
        "/dl/v1.1.0/testcraft-web-1.1.0.zip",
        Behavior {
            truncate_at: Some(100),
            ..Default::default()
        },
    );
    assert_failed(&update(&e), "download", "interrupted after 100 bytes");
    assert_eq!(
        gh.hits("/dl/v1.1.0/testcraft-web-1.1.0.zip"),
        2,
        "bounded retry: max_retries = 1"
    );
    assert_eq!(
        std::fs::read_dir(e.cfg.paths.work.join("downloads"))
            .unwrap()
            .count(),
        0,
        "partial download removed"
    );
    assert_eq!(e.served_version().as_deref(), Some("1.0.0"));

    gh.clear_behaviors();
    gh.behave(
        "/repos/storytold/testcraft/releases",
        Behavior {
            status: Some(403),
            headers: vec![
                ("X-RateLimit-Remaining".into(), "0".into()),
                ("X-RateLimit-Reset".into(), "4102444800".into()),
            ],
            ..Default::default()
        },
    );
    assert_failed(&update(&e), "discover", "rate limit");
    let st = Updater::new(&e.cfg).store.load("testcraft").unwrap();
    assert_eq!(
        st.retry_after,
        Some(4_102_444_800),
        "waits for the advertised reset"
    );
    let auto = Updater::new(&e.cfg).update_app(e.cfg.app("testcraft").unwrap(), false);
    assert!(
        matches!(auto, Outcome::Skipped(_)),
        "automatic runs honour the backoff: {auto}"
    );

    // Offline: GitHub unreachable entirely.
    gh.clear_behaviors();
    gh.behave(
        "/repos/storytold/testcraft/releases",
        Behavior {
            status: Some(503),
            ..Default::default()
        },
    );
    assert_failed(&update(&e), "discover", "HTTP 503");
    assert_eq!(e.served_version().as_deref(), Some("1.0.0"));

    gh.clear_behaviors();
    assert_eq!(kind(&update(&e)), installed("1.1.0"));
}

#[test]
fn disk_space_budget_is_checked_before_downloading() {
    let gh = FakeGitHub::start();
    let e = env_with(&gh, "", "min_free_space = \"1000000GiB\"", "");
    gh.publish(
        REPO,
        "v1.0.0",
        release_zip(&e.root(), "1.0.0", &[]),
        PublishOpts::default(),
    );
    assert_failed(&update(&e), "space", "insufficient disk space in WORK_DIR");
    assert_eq!(gh.hits("/dl/v1.0.0/testcraft-web-1.0.0.zip"), 0);
    let e = env_with(&gh, "", "min_free_space = 0\nmax_download = 10", "");
    assert_failed(&update(&e), "limits", "above max_download");
}

#[test]
fn renamed_artifacts_and_prerelease_tags_follow_the_manifest() {
    let gh = FakeGitHub::start();
    let e = env(&gh);
    gh.publish(
        REPO,
        "v0.9.0",
        release_zip(&e.root(), "0.9.0", &[]),
        PublishOpts {
            asset_name: Some("oldcraft-web-0.9.0.zip".into()),
            ..Default::default()
        },
    );
    gh.publish(
        REPO,
        "v1.0.0-rc.1",
        release_zip(&e.root(), "1.0.0-rc.1", &[]),
        PublishOpts::default(),
    );
    assert_eq!(
        kind(&update(&e)),
        installed("0.9.0"),
        "old artifact name accepted; rc tag ignored"
    );
    gh.publish(
        REPO,
        "v1.0.0",
        release_zip(&e.root(), "1.0.0", &[]),
        PublishOpts::default(),
    );
    assert_eq!(
        kind(&update(&e)),
        installed("1.0.0"),
        "new artifact name accepted"
    );
}

#[test]
fn rollback_blocks_until_allowed_or_superseded() {
    let gh = FakeGitHub::start();
    // Rolling back needs an older release on disk: keep two.
    let e = env_with(&gh, "keep_latest = 2", "min_free_space = 0", "");
    let app = e.cfg.app("testcraft").unwrap();
    gh.publish(
        REPO,
        "v1.0.0",
        release_zip(&e.root(), "1.0.0", &[]),
        PublishOpts::default(),
    );
    update(&e);
    gh.publish(
        REPO,
        "v1.1.0",
        release_zip(&e.root(), "1.1.0", &[]),
        PublishOpts::default(),
    );
    update(&e);

    let u = Updater::new(&e.cfg);
    let msg = u.rollback(app, None).unwrap();
    assert!(msg.contains("from 1.1.0 to 1.0.0"), "{msg}");
    assert_eq!(e.served_version().as_deref(), Some("1.0.0"));

    let o = update(&e);
    assert!(
        matches!(&o, Outcome::Skipped(why) if why.contains("blocked")),
        "{o}"
    );
    assert_eq!(e.served_version().as_deref(), Some("1.0.0"));

    u.allow(app, "1.1.0").unwrap();
    assert_eq!(
        kind(&update(&e)),
        ("activated", "1.1.0".into()),
        "allowed release reactivated without download"
    );

    u.rollback(app, Some("1.0.0")).unwrap();
    gh.publish(
        REPO,
        "v1.2.0",
        release_zip(&e.root(), "1.2.0", &[]),
        PublishOpts::default(),
    );
    assert_eq!(
        kind(&update(&e)),
        installed("1.2.0"),
        "a newer release supersedes the block"
    );

    assert!(
        u.rollback(app, Some("9.9.9"))
            .unwrap_err()
            .to_string()
            .contains("not a retained release")
    );
}

#[test]
fn pins_hold_a_version_and_survive_offline_starts() {
    let gh = FakeGitHub::start();
    let e = env(&gh);
    let app = e.cfg.app("testcraft").unwrap();
    gh.publish(
        REPO,
        "v1.0.0",
        release_zip(&e.root(), "1.0.0", &[]),
        PublishOpts::default(),
    );
    gh.publish(
        REPO,
        "v1.1.0",
        release_zip(&e.root(), "1.1.0", &[]),
        PublishOpts::default(),
    );
    let u = Updater::new(&e.cfg);
    u.prepare().unwrap();
    assert_eq!(kind(&u.pin(app, "v1.0.0").unwrap()), installed("1.0.0"));
    assert_eq!(kind(&update(&e)), ("up-to-date", "1.0.0".into()));
    gh.behave(
        "/repos/storytold/testcraft/releases",
        Behavior {
            status: Some(503),
            ..Default::default()
        },
    );
    assert!(
        matches!(update(&e), Outcome::UpToDate(Some(_))),
        "pinned and installed: no network needed"
    );
    gh.clear_behaviors();
    assert!(
        u.rollback(app, None)
            .unwrap_err()
            .to_string()
            .contains("pinned")
    );
    u.unpin(app).unwrap();
    assert_eq!(kind(&update(&e)), installed("1.1.0"));

    let pinned_cfg = env_with(&gh, "pinned_version = \"1.0.0\"", "min_free_space = 0", "");
    assert_eq!(kind(&update(&pinned_cfg)), installed("1.0.0"), "config pin");
    assert!(
        Updater::new(&pinned_cfg.cfg)
            .unpin(pinned_cfg.cfg.app("testcraft").unwrap())
            .unwrap()
            .contains("still pinned")
    );
}

#[test]
fn retention_keeps_active_pinned_and_recent_releases() {
    let gh = FakeGitHub::start();
    let e = env_with(
        &gh,
        "keep_latest = 2\nkeep_days = 0",
        "min_free_space = 0",
        "",
    );
    for v in ["1.0.0", "1.1.0", "1.2.0", "1.3.0"] {
        gh.publish(
            REPO,
            &format!("v{v}"),
            release_zip(&e.root(), v, &[]),
            PublishOpts::default(),
        );
        update(&e);
        std::thread::sleep(Duration::from_millis(1100)); // distinct installed_at seconds
    }
    let installed: Vec<_> = layout::installed_dirs(&e.cfg, e.cfg.app("testcraft").unwrap())
        .into_iter()
        .map(|(v, _)| v)
        .collect();
    assert_eq!(installed, ["1.2.0", "1.3.0"]);
    let st = Updater::new(&e.cfg).store.load("testcraft").unwrap();
    assert_eq!(
        st.installed.keys().cloned().collect::<Vec<_>>(),
        ["1.2.0", "1.3.0"]
    );
    assert!(
        Updater::new(&e.cfg)
            .store
            .history(Some("testcraft"), 50)
            .iter()
            .any(|h| h.action == "retention" && h.version.as_deref() == Some("1.0.0"))
    );
}

#[test]
fn keep_latest_counts_the_active_release_first() {
    let gh = FakeGitHub::start();
    let e = env(&gh); // defaults: keep one release
    let app = e.cfg.app("testcraft").unwrap();
    for v in ["1.0.0", "1.1.0"] {
        gh.publish(
            REPO,
            &format!("v{v}"),
            release_zip(&e.root(), v, &[]),
            PublishOpts::default(),
        );
    }
    assert_eq!(kind(&update(&e)), installed("1.1.0"));
    std::thread::sleep(Duration::from_millis(1100)); // distinct installed_at seconds
    let u = Updater::new(&e.cfg);
    u.pin(app, "1.0.0").unwrap();
    assert_eq!(e.current().as_deref(), Some("1.0.0"));
    u.unpin(app).unwrap();
    update(&e);
    assert_eq!(e.current().as_deref(), Some("1.1.0"));
    // 1.0.0 was installed last, but the active 1.1.0 is the one kept.
    u.apply_retention(app).unwrap();
    assert_eq!(
        u.store
            .load("testcraft")
            .unwrap()
            .installed
            .keys()
            .collect::<Vec<_>>(),
        ["1.1.0"]
    );
    assert!(!e.release_dir("1.0.0").exists());
}

#[test]
fn reconcile_recovers_from_interrupted_operations() {
    let gh = FakeGitHub::start();
    let e = env(&gh);
    gh.publish(
        REPO,
        "v1.0.0",
        release_zip(&e.root(), "1.0.0", &[]),
        PublishOpts::default(),
    );
    gh.publish(
        REPO,
        "v1.1.0",
        release_zip(&e.root(), "1.1.0", &[]),
        PublishOpts::default(),
    );
    update(&e);
    let u = Updater::new(&e.cfg);
    let app = e.cfg.app("testcraft").unwrap();

    // Crash mid-extraction: staging leftovers and a partial download.
    std::fs::create_dir_all(e.cfg.paths.staging().join("testcraft-1.2.0-abc/site")).unwrap();
    std::fs::write(e.cfg.paths.work.join("downloads/x.part"), b"partial").unwrap();
    // Crash after promotion but before the state save: a marked release unknown to state.
    let mut st = u.store.load("testcraft").unwrap();
    st.installed.clear();
    st.active = None;
    u.store.save("testcraft", &st).unwrap();
    // An unmarked directory (not ours) must be left alone.
    std::fs::create_dir_all(e.release_dir("0.0.1")).unwrap();

    let notes = layout::reconcile(&e.cfg, &u.store).unwrap();
    assert!(
        notes.iter().any(|n| n.contains("removed interrupted work")),
        "{notes:?}"
    );
    assert!(
        notes.iter().any(|n| n.contains("recorded release 1.1.0")),
        "{notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|n| n.contains("0.0.1 has no release marker")),
        "{notes:?}"
    );
    assert_eq!(std::fs::read_dir(e.cfg.paths.staging()).unwrap().count(), 0);
    let st = u.store.load("testcraft").unwrap();
    assert_eq!(
        st.active.as_deref(),
        Some("1.1.0"),
        "state follows the atomic pointer"
    );
    assert!(e.release_dir("0.0.1").is_dir());

    // Dangling pointer (release deleted by hand): falls back to an installed release.
    layout::activate(&e.cfg, app, "1.1.0").unwrap();
    craft_host::fsutil::remove_tree(&e.release_dir("1.1.0")).unwrap();
    let notes = layout::reconcile(&e.cfg, &u.store).unwrap();
    assert!(notes.iter().any(|n| n.contains("dangling")), "{notes:?}");
    assert_eq!(e.current(), None, "nothing complete left to activate");
    assert!(u.store.load("testcraft").unwrap().installed.is_empty());
}

#[test]
fn concurrent_cli_updates_are_serialized_by_the_lock() {
    let gh = FakeGitHub::start();
    let e = env(&gh);
    gh.publish(
        REPO,
        "v1.0.0",
        release_zip(&e.root(), "1.0.0", &[]),
        PublishOpts::default(),
    );
    gh.behave(
        "/dl/v1.0.0/testcraft-web-1.0.0.zip",
        Behavior {
            delay: Some(Duration::from_millis(1500)),
            ..Default::default()
        },
    );
    let spawn = || {
        let mut c = Command::new(env!("CARGO_BIN_EXE_craft-host"));
        c.arg("update")
            .arg("testcraft")
            .envs(e.paths_env())
            .env("FILE_UMASK", "0022");
        c.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let a = spawn();
    std::thread::sleep(Duration::from_millis(300));
    let b = spawn();
    let outs = [a.wait_with_output().unwrap(), b.wait_with_output().unwrap()];
    let ok = outs.iter().filter(|o| o.status.success()).count();
    assert_eq!(ok, 1, "exactly one update runs: {outs:?}");
    let loser = outs.iter().find(|o| !o.status.success()).unwrap();
    assert!(
        String::from_utf8_lossy(&loser.stderr).contains("another update operation holds the lock"),
        "{loser:?}"
    );
    assert_eq!(e.served_version().as_deref(), Some("1.0.0"));
}

#[test]
fn files_follow_the_configured_umask() {
    let gh = FakeGitHub::start();
    let e = env(&gh);
    gh.publish(
        REPO,
        "v1.0.0",
        release_zip(&e.root(), "1.0.0", &[]),
        PublishOpts::default(),
    );
    let out = Command::new(env!("CARGO_BIN_EXE_craft-host"))
        .arg("update")
        .envs(e.paths_env())
        .env("FILE_UMASK", "0027")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let mode = |p: std::path::PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode(e.release_dir("1.0.0").join("index.html")),
        0o440,
        "0666 & ~0027, then write bits removed"
    );
    assert_eq!(mode(e.release_dir("1.0.0")), 0o550);
    assert_eq!(mode(e.cfg.paths.state.join("apps/testcraft.json")), 0o640);
}

#[test]
fn idle_activation_waits_for_quiet_apps_and_survives_restarts() {
    use craft_host::activity::Activity;
    use std::sync::Arc;
    let gh = FakeGitHub::start();
    let e = env_with(
        &gh,
        "activation = \"idle\"\nidle_after = 2\nkeep_latest = 5",
        "min_free_space = 0",
        "",
    );
    let app = e.cfg.app("testcraft").unwrap();
    let activity = Arc::new(Activity::new());
    let u = Updater::new(&e.cfg).with_activity(activity.clone());
    u.prepare().unwrap();
    gh.publish(
        REPO,
        "v1.0.0",
        release_zip(&e.root(), "1.0.0", &[]),
        PublishOpts::default(),
    );
    assert_eq!(
        kind(&u.update_app(app, false)),
        installed("1.0.0"),
        "first install goes live at once"
    );

    activity.record("testcraft", Some("1.0.0"));
    gh.publish(
        REPO,
        "v1.1.0",
        release_zip(&e.root(), "1.1.0", &[]),
        PublishOpts::default(),
    );
    assert_eq!(kind(&u.update_app(app, false)), ("staged", "1.1.0".into()));
    assert_eq!(e.served_version().as_deref(), Some("1.0.0"));
    let s = status::build(&e.cfg, &u.store, true);
    assert_eq!(s.apps[0].pending.as_deref(), Some("1.1.0"));
    assert!(
        !s.apps[0].update_available,
        "a downloaded release is not reported as available"
    );
    assert_eq!(
        kind(&u.update_app(app, false)),
        ("staged", "1.1.0".into()),
        "no second download"
    );

    // A restart keeps the pending release; a request keeps the app busy.
    layout::reconcile(&e.cfg, &u.store).unwrap();
    assert_eq!(
        u.store.load("testcraft").unwrap().pending.as_deref(),
        Some("1.1.0")
    );
    activity.record("testcraft", None);
    assert_eq!(u.apply_pending(app, false).unwrap(), None);
    std::thread::sleep(Duration::from_millis(2100));
    assert_eq!(
        u.apply_pending(app, false).unwrap().as_deref(),
        Some("1.1.0")
    );
    assert_eq!(e.served_version().as_deref(), Some("1.1.0"));
    assert_eq!(u.store.load("testcraft").unwrap().pending, None);

    // Operators bypass the idle policy; rollback and pins discard a pending release.
    activity.record("testcraft", None);
    gh.publish(
        REPO,
        "v1.2.0",
        release_zip(&e.root(), "1.2.0", &[]),
        PublishOpts::default(),
    );
    assert_eq!(kind(&u.update_app(app, true)), installed("1.2.0"));
    gh.publish(
        REPO,
        "v1.3.0",
        release_zip(&e.root(), "1.3.0", &[]),
        PublishOpts::default(),
    );
    assert_eq!(kind(&u.update_app(app, false)), ("staged", "1.3.0".into()));
    u.rollback(app, Some("1.1.0")).unwrap();
    assert_eq!(u.store.load("testcraft").unwrap().pending, None);
    assert_eq!(u.apply_pending(app, true).unwrap(), None);
    assert_eq!(e.served_version().as_deref(), Some("1.1.0"));

    // A process without activity data (the CLI) never assumes the app is idle.
    let cli = Updater::new(&e.cfg);
    gh.publish(
        REPO,
        "v1.4.0",
        release_zip(&e.root(), "1.4.0", &[]),
        PublishOpts::default(),
    );
    assert_eq!(
        kind(&cli.update_app(app, false)),
        ("staged", "1.4.0".into())
    );
}

#[test]
fn retention_keeps_releases_that_open_tabs_recently_used() {
    use craft_host::activity::Activity;
    use std::sync::Arc;
    let gh = FakeGitHub::start();
    let e = env_with(
        &gh,
        "keep_latest = 1\nkeep_days = 0",
        "min_free_space = 0",
        "[retention]\nkeep_recently_used = \"1h\"\n",
    );
    let app = e.cfg.app("testcraft").unwrap();
    let activity = Arc::new(Activity::new());
    let u = Updater::new(&e.cfg).with_activity(activity.clone());
    u.prepare().unwrap();
    for v in ["1.0.0", "1.1.0"] {
        gh.publish(
            REPO,
            &format!("v{v}"),
            release_zip(&e.root(), v, &[]),
            PublishOpts::default(),
        );
    }
    gh.publish(
        REPO,
        "v1.0.0",
        release_zip(&e.root(), "1.0.0", &[]),
        PublishOpts::default(),
    );
    u.pin(app, "1.0.0").unwrap();
    u.unpin(app).unwrap();
    activity.record("testcraft", Some("1.0.0"));
    std::thread::sleep(Duration::from_millis(1100));
    assert_eq!(kind(&u.update_app(app, true)), installed("1.1.0"));
    assert!(e.release_dir("1.0.0").is_dir(), "a tab used 1.0.0 recently");

    let fresh = Updater::new(&e.cfg);
    fresh.apply_retention(app).unwrap();
    assert!(
        !e.release_dir("1.0.0").exists(),
        "without recent use the policy removes it"
    );
}

#[test]
fn backfill_adds_copies_to_older_releases_and_cleans_interrupted_runs() {
    let gh = FakeGitHub::start();
    let e = env(&gh);
    let big = format!("export const d = \"{}\";\n", "x".repeat(4000));
    gh.publish(
        REPO,
        "v1.0.0",
        release_zip(&e.root(), "1.0.0", &[("big.js", big.as_bytes())]),
        PublishOpts::default(),
    );
    update(&e);
    let dir = e.release_dir("1.0.0");
    // Simulate a release from an older version, plus a crash during an earlier backfill.
    craft_host::fsutil::restore_owner_write(&dir).unwrap();
    for c in ["big.js.gz", "big.js.br"] {
        std::fs::remove_file(dir.join(c)).unwrap();
    }
    std::fs::write(dir.join(".big.js.gz.ab12.tmp"), b"partial").unwrap();
    craft_host::fsutil::strip_write_bits(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    let done = layout::backfill_precompressed(&e.cfg).unwrap();
    assert_eq!(done, ["testcraft/1.0.0"]);
    assert!(dir.join("big.js.gz").is_file() && dir.join("big.js.br").is_file());
    assert!(!dir.join(".big.js.gz.ab12.tmp").exists());
    assert_eq!(
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o222,
        0,
        "read-only again"
    );
    assert!(
        layout::backfill_precompressed(&e.cfg).unwrap().is_empty(),
        "nothing left to do"
    );
}

#[test]
fn restarts_and_retries_only_query_github_for_apps_that_are_due() {
    use craft_host::daemon::{cycle, first_run};
    use craft_host::timeutil::now_epoch;
    let gh = FakeGitHub::start();
    let e = env(&gh);
    gh.publish(
        REPO,
        "v1.0.0",
        release_zip(&e.root(), "1.0.0", &[]),
        PublishOpts::default(),
    );
    let route = format!("/repos/{REPO}/releases");
    let u = Updater::new(&e.cfg);
    u.prepare().unwrap();
    let interval = e.cfg.updater.check_interval_secs as i64;

    // Never checked: due at once.
    assert!(first_run(&e.cfg, &u.store) <= now_epoch());
    cycle(&e.cfg, &u, false);
    assert_eq!(e.current().as_deref(), Some("1.0.0"));
    let after_install = gh.hits(&route);

    // A restart right after a good check continues the schedule instead of checking again.
    assert!(first_run(&e.cfg, &u.store) >= now_epoch() + interval - 5);
    cycle(&e.cfg, &u, false);
    assert_eq!(gh.hits(&route), after_install, "not due: no request");

    // A failed check waits for its retry deadline (backoff or rate-limit reset).
    let mut st = u.store.load("testcraft").unwrap();
    st.last_check_ok = Some(false);
    st.retry_after = Some(now_epoch() + 600);
    u.store.save("testcraft", &st).unwrap();
    let first = first_run(&e.cfg, &u.store);
    assert!(
        (now_epoch() + 595..=now_epoch() + 600).contains(&first),
        "{first}"
    );
    cycle(&e.cfg, &u, false);
    assert_eq!(
        gh.hits(&route),
        after_install,
        "waiting for the retry deadline"
    );

    // Deadline passed: due again, one request.
    st.retry_after = Some(now_epoch() - 1);
    u.store.save("testcraft", &st).unwrap();
    assert!(first_run(&e.cfg, &u.store) <= now_epoch());
    cycle(&e.cfg, &u, false);
    assert_eq!(gh.hits(&route), after_install + 1);

    // Manual cycles (CLI `update`, /admin) always check.
    cycle(&e.cfg, &u, true);
    assert_eq!(gh.hits(&route), after_install + 2);
}

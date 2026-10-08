//! Persistent updater state: per-app records, update history, heartbeat and the operation lock.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::fsutil::{FILE_MODE, atomic_write, atomic_write_json, make_dirs, read_json};
use crate::timeutil::{now_epoch, now_iso};

const HISTORY_MAX_BYTES: u64 = 2 << 20;
const HISTORY_KEEP_LINES: usize = 5000;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct InstalledRelease {
    pub tag: String,
    pub asset: String,
    pub sha256: String,
    pub installed_at: String,
    #[serde(default)]
    pub size: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Failure {
    pub at: String,
    pub stage: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SeenRelease {
    pub version: String,
    pub tag: String,
    pub published_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AppState {
    pub installed: BTreeMap<String, InstalledRelease>,
    pub active: Option<String>,
    /// Installed release waiting for idle activation.
    pub pending: Option<String>,
    /// Pin set with the CLI; takes precedence over `pinned_version` in config.toml.
    pub pinned: Option<String>,
    /// Versions an operator rolled back from; not reinstalled automatically until allowed.
    pub blocked: Vec<String>,
    pub latest_seen: Option<SeenRelease>,
    pub skipped: Vec<String>,
    pub last_check: Option<String>,
    pub last_check_ok: Option<bool>,
    pub last_update: Option<String>,
    pub last_error: Option<Failure>,
    pub failed_versions: BTreeMap<String, Failure>,
    /// Epoch seconds before which automatic attempts wait (bounded backoff after failures).
    pub retry_after: Option<i64>,
    pub consecutive_failures: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub at: String,
    pub app: String,
    pub action: String,
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Heartbeat {
    pub at: String,
    pub epoch: i64,
    pub pid: u32,
    pub phase: String,
}

pub struct Store {
    pub root: PathBuf,
}

#[derive(Debug)]
pub struct LockGuard {
    file: File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = self.file.set_len(0);
        let _ = self.file.unlock();
    }
}

impl Store {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    fn apps_dir(&self) -> PathBuf {
        self.root.join("apps")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.root.join("locks").join("updater.lock")
    }

    fn history_path(&self) -> PathBuf {
        self.root.join("history.jsonl")
    }

    fn heartbeat_path(&self) -> PathBuf {
        self.root.join("heartbeat.json")
    }

    pub fn ensure(&self) -> Result<()> {
        make_dirs(&self.apps_dir())
            .with_context(|| format!("create {}", self.apps_dir().display()))?;
        make_dirs(&self.root.join("locks")).context("create lock directory")?;
        Ok(())
    }

    pub fn load(&self, app: &str) -> Result<AppState> {
        Ok(read_json(&self.apps_dir().join(format!("{app}.json")))?.unwrap_or_default())
    }

    pub fn save(&self, app: &str, state: &AppState) -> Result<()> {
        atomic_write_json(&self.apps_dir().join(format!("{app}.json")), state)
    }

    pub fn record(
        &self,
        app: &str,
        action: &str,
        outcome: &str,
        version: Option<&str>,
        message: &str,
    ) -> Result<()> {
        let entry = HistoryEntry {
            at: now_iso(),
            app: app.into(),
            action: action.into(),
            outcome: outcome.into(),
            version: version.map(Into::into),
            message: message.into(),
        };
        let path = self.history_path();
        let mut f = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(FILE_MODE)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        let mut line = serde_json::to_vec(&entry)?;
        line.push(b'\n');
        f.write_all(&line)?;
        if f.metadata()?.len() > HISTORY_MAX_BYTES {
            let text = fs::read_to_string(&path)?;
            let lines: Vec<&str> = text.lines().collect();
            let keep = lines[lines.len().saturating_sub(HISTORY_KEEP_LINES)..].join("\n") + "\n";
            atomic_write(&path, keep.as_bytes())?;
        }
        Ok(())
    }

    pub fn history(&self, app: Option<&str>, limit: usize) -> Vec<HistoryEntry> {
        let Ok(text) = fs::read_to_string(self.history_path()) else {
            return Vec::new();
        };
        text.lines()
            .rev()
            .filter_map(|l| serde_json::from_str::<HistoryEntry>(l).ok())
            .filter(|e| app.is_none_or(|a| e.app == a))
            .take(limit)
            .collect()
    }

    pub fn beat(&self, phase: &str) -> Result<()> {
        atomic_write_json(
            &self.heartbeat_path(),
            &Heartbeat {
                at: now_iso(),
                epoch: now_epoch(),
                pid: std::process::id(),
                phase: phase.into(),
            },
        )
    }

    pub fn heartbeat(&self) -> Option<Heartbeat> {
        read_json(&self.heartbeat_path()).ok().flatten()
    }

    /// Exclusive advisory lock serializing automatic and manual operations across processes
    /// and containers sharing STATE_DIR.
    pub fn lock(&self, wait: Duration, holder: &str) -> Result<LockGuard> {
        let path = self.lock_path();
        make_dirs(path.parent().unwrap())?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(FILE_MODE)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        let deadline = Instant::now() + wait;
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        let mut owner = String::new();
                        let _ = file.read_to_string(&mut owner);
                        let owner = owner.trim();
                        bail!(
                            "another update operation holds the lock{}; try again later",
                            if owner.is_empty() {
                                String::new()
                            } else {
                                format!(" ({owner})")
                            }
                        );
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
                Err(TryLockError::Error(e)) => return Err(e).context("lock state directory"),
            }
        }
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        write!(
            file,
            "{holder} pid={} since={}",
            std::process::id(),
            now_iso()
        )?;
        Ok(LockGuard { file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_excludes_a_second_holder_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let guard = store.lock(Duration::ZERO, "first").unwrap();
        let err = store
            .lock(Duration::from_millis(300), "second")
            .unwrap_err()
            .to_string();
        assert!(err.contains("first"), "{err}");
        drop(guard);
        store.lock(Duration::ZERO, "second").unwrap();
    }

    #[test]
    fn state_round_trips_and_history_is_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.ensure().unwrap();
        let st = AppState {
            active: Some("0.5.0".into()),
            blocked: vec!["0.6.0".into()],
            ..Default::default()
        };
        store.save("photocraft", &st).unwrap();
        assert_eq!(store.load("photocraft").unwrap(), st);
        assert_eq!(store.load("other").unwrap(), AppState::default());
        store
            .record("a", "update", "ok", Some("1.0.0"), "")
            .unwrap();
        store
            .record("b", "update", "failed", Some("2.0.0"), "boom")
            .unwrap();
        let h = store.history(None, 10);
        assert_eq!(h[0].app, "b");
        assert_eq!(store.history(Some("a"), 10).len(), 1);
    }
}

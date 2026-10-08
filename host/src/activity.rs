//! Per-app and per-release request activity, used for idle activation and to keep releases that
//! open tabs may still load files from.
//!
//! Requests are the only signal the server has. Browser apps load once and may then run for
//! hours without requesting anything, so "idle" is a heuristic, not proof that nobody uses an app.

use std::collections::HashMap;

use parking_lot::Mutex;

use crate::timeutil::now_epoch;

#[derive(Default)]
pub struct Activity {
    started: i64,
    apps: Mutex<HashMap<String, i64>>,
    releases: Mutex<HashMap<(String, String), i64>>,
}

impl Activity {
    pub fn new() -> Self {
        Self {
            started: now_epoch(),
            ..Default::default()
        }
    }

    pub fn record(&self, app: &str, version: Option<&str>) {
        let now = now_epoch();
        self.apps.lock().insert(app.to_string(), now);
        if let Some(v) = version {
            self.releases
                .lock()
                .insert((app.to_string(), v.to_string()), now);
        }
    }

    /// Seconds since the app was last requested. Before the first request, time since this
    /// process started counts: after a restart nothing is known about earlier activity.
    pub fn idle_secs(&self, app: &str) -> i64 {
        let last = self.apps.lock().get(app).copied().unwrap_or(self.started);
        now_epoch() - last
    }

    pub fn last_app_request(&self, app: &str) -> Option<i64> {
        self.apps.lock().get(app).copied()
    }

    pub fn release_used_within(&self, app: &str, version: &str, secs: u64) -> bool {
        self.releases
            .lock()
            .get(&(app.to_string(), version.to_string()))
            .is_some_and(|t| now_epoch() - t <= secs as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_time_counts_from_start_until_first_request() {
        let a = Activity {
            started: now_epoch() - 100,
            ..Default::default()
        };
        assert!(a.idle_secs("photocraft") >= 100);
        a.record("photocraft", Some("0.5.0"));
        assert!(a.idle_secs("photocraft") <= 1);
        assert!(a.release_used_within("photocraft", "0.5.0", 60));
        assert!(!a.release_used_within("photocraft", "0.3.0", 60));
    }
}

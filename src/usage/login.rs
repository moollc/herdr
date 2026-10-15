//! Hourly, noninteractive Cursor login refresh. No credentials enter this module.
use super::{atomic_json, read_json, UsageConfig};
pub(super) use crate::platform::usage::CursorLoginResult;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, time::Duration};
const MIN_INTERVAL: u64 = 3600;
pub(super) const TIMEOUT: Duration = Duration::from_secs(20);
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct RefreshState {
    pub no_consent: bool,
    last_attempt: Option<u64>,
}
fn marker(config: &UsageConfig) -> PathBuf {
    config.snapshot_file.with_extension("cursor-login")
}
impl RefreshState {
    pub fn new(config: &UsageConfig) -> Self {
        Self {
            no_consent: false,
            last_attempt: read_json(&marker(config)).ok(),
        }
    }
    /// Apply the worker's consent preflight before touching the hourly lease.
    /// Return whether consent was newly granted so the next reading can retry now.
    pub fn set_consent(&mut self, enabled: bool) -> bool {
        let granted = self.no_consent && enabled;
        self.no_consent = !enabled;
        granted
    }
    pub fn attempt(
        &mut self,
        config: &UsageConfig,
        now: u64,
        run: impl FnOnce(Duration) -> CursorLoginResult,
    ) -> Option<CursorLoginResult> {
        if self.no_consent {
            return None;
        }
        let path = marker(config);
        // Share the short read/compare/write critical section with other Herdr processes.
        // Drop the lock before launching the importer; the saved timestamp is the lease.
        let lock = match crate::platform::usage::lock_statusline(&path) {
            Ok(lock) => lock,
            Err(_) => return None,
        };
        let persisted: Option<u64> = read_json(&path).ok();
        let last = self.last_attempt.max(persisted);
        if last.is_some_and(|last| now.saturating_sub(last) < MIN_INTERVAL) {
            return None;
        }
        if atomic_json(&path, &now).is_err() {
            return None;
        }
        self.last_attempt = Some(now);
        drop(lock);
        let result = run(TIMEOUT);
        if result == CursorLoginResult::Exited(3) {
            self.no_consent = true;
        }
        Some(result)
    }
}

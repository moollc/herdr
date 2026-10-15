//! Account allowance readings. Refresh on a worker thread; rendering only sees snapshots.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
pub mod claude;
pub mod cli;
pub mod codex;
pub mod countdown;
pub mod cursor;
mod login;
pub mod palette;
pub mod statusline;

const CURSOR_MIN_INTERVAL_SECS: u64 = 900;

fn cursor_fetch_file(config: &UsageConfig) -> PathBuf {
    config.snapshot_file.with_extension("cursor-fetch")
}
fn cursor_fetch_time(config: &UsageConfig) -> Option<u64> {
    read_json(&cursor_fetch_file(config)).ok()
}
fn cursor_due(config: &UsageConfig, last: Option<u64>, now: u64) -> bool {
    last.is_none_or(|last| {
        now.saturating_sub(last)
            >= config
                .cursor_min_interval_secs
                .max(CURSOR_MIN_INTERVAL_SECS)
    })
}
pub(super) fn cursor_fetch_due(config: &UsageConfig, now: u64) -> bool {
    cursor_due(config, cursor_fetch_time(config), now)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
    Cursor,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowKind {
    FiveHour,
    Weekly,
    Monthly,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageProblem {
    NoData,
    NoLogin,
    LoginExpired,
    FormatChanged,
    Offline,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    pub kind: WindowKind,
    pub left_pct: Option<u8>,
    pub resets_at: Option<u64>,
    pub note: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderUsage {
    pub provider: Provider,
    pub windows: Vec<UsageWindow>,
    pub seen_at: Option<u64>,
    pub source: String,
    pub problem: Option<UsageProblem>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub version: u32,
    pub taken_at: u64,
    pub providers: Vec<ProviderUsage>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageConfig {
    pub codex_home: PathBuf,
    pub claude_state_file: PathBuf,
    pub snapshot_file: PathBuf,
    pub cursor_keychain_service: String,
    pub cursor_min_interval_secs: u64,
    pub fixture: Option<PathBuf>,
}
impl Default for UsageConfig {
    fn default() -> Self {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let state = crate::config::state_dir().join("usage");
        Self {
            codex_home: std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex")),
            claude_state_file: state.join("claude.json"),
            snapshot_file: state.join("usage.json"),
            cursor_keychain_service: "herdr-cursor-usage".into(),
            cursor_min_interval_secs: CURSOR_MIN_INTERVAL_SECS,
            fixture: std::env::var_os("HERDR_USAGE_FIXTURE").map(PathBuf::from),
        }
    }
}
impl UsageConfig {
    pub fn configured() -> Self {
        let mut config = Self::default();
        let display = crate::config::Config::load().config.ui.usage;
        if let Some(path) = display.codex_home {
            config.codex_home = if let Some(rest) = path.strip_prefix("~/") {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest)
            } else {
                PathBuf::from(path)
            };
        }
        config
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageState {
    config: UsageConfig,
    last_cursor_fetch: Option<u64>,
    cursor_result: Option<ProviderUsage>,
    cursor_good: Option<ProviderUsage>,
    cursor_login: login::RefreshState,
    alerts: HashMap<(Provider, WindowKind), Option<u64>>,
}
impl UsageState {
    pub fn new(config: UsageConfig) -> Self {
        let cached = read_json::<UsageSnapshot>(&config.snapshot_file).ok();
        let cursor_result = cached
            .as_ref()
            .and_then(|s| s.providers.iter().find(|p| p.provider == Provider::Cursor))
            .cloned();
        let last_cursor_fetch = cursor_fetch_time(&config);
        let cursor_good = cursor_result
            .as_ref()
            .filter(|p| p.problem.is_none() || p.problem == Some(UsageProblem::Offline))
            .cloned();
        let cursor_login = login::RefreshState::new(&config);
        Self {
            config,
            cursor_login,
            last_cursor_fetch,
            cursor_result,
            cursor_good,
            alerts: HashMap::new(),
        }
    }
    /// Fetch readings without importing a login or spending its hourly attempt.
    pub fn refresh(&mut self, want: &[Provider], now: u64) -> UsageSnapshot {
        self.refresh_with_sources_and_publish(
            want,
            now,
            |service| cursor::fetch(service, now),
            false,
            |_| unreachable!("read-only refresh cannot run login"),
            |_| {},
        )
    }
    /// App worker only: publish local readings before the optional login child.
    pub(crate) fn refresh_worker(
        &mut self,
        want: &[Provider],
        now: u64,
        publish: impl FnMut(UsageSnapshot),
    ) -> UsageSnapshot {
        self.config.codex_home = UsageConfig::configured().codex_home;
        self.refresh_worker_with_sources(
            want,
            now,
            crate::config::Config::load().config.ui.usage.cursor_login,
            |service| cursor::fetch(service, now),
            crate::platform::usage::refresh_cursor_login,
            publish,
        )
    }
    fn refresh_worker_with_sources(
        &mut self,
        want: &[Provider],
        now: u64,
        consent: bool,
        fetch: impl FnMut(&str) -> ProviderUsage,
        command: impl FnOnce(std::time::Duration) -> login::CursorLoginResult,
        publish: impl FnMut(UsageSnapshot),
    ) -> UsageSnapshot {
        if self.cursor_login.set_consent(consent) {
            // A newly allowed login must not wait for the cached missing-login poll.
            self.last_cursor_fetch = None;
        }
        self.refresh_with_sources_and_publish(want, now, fetch, true, command, publish)
    }
    #[cfg(test)]
    fn refresh_with_cursor_fetch(
        &mut self,
        want: &[Provider],
        now: u64,
        fetch: impl FnMut(&str) -> ProviderUsage,
    ) -> UsageSnapshot {
        self.refresh_with_sources(want, now, fetch, |_| login::CursorLoginResult::Failed)
    }
    #[cfg(test)]
    fn refresh_with_sources(
        &mut self,
        want: &[Provider],
        now: u64,
        fetch: impl FnMut(&str) -> ProviderUsage,
        command: impl FnOnce(std::time::Duration) -> login::CursorLoginResult,
    ) -> UsageSnapshot {
        self.refresh_with_sources_and_publish(want, now, fetch, true, command, |_| {})
    }
    fn refresh_with_sources_and_publish(
        &mut self,
        want: &[Provider],
        now: u64,
        mut fetch: impl FnMut(&str) -> ProviderUsage,
        allow_login: bool,
        command: impl FnOnce(std::time::Duration) -> login::CursorLoginResult,
        mut publish: impl FnMut(UsageSnapshot),
    ) -> UsageSnapshot {
        if let Some(path) = &self.config.fixture {
            let snapshot = read_json::<UsageSnapshot>(path).unwrap_or_else(|_| UsageSnapshot {
                version: 1,
                taken_at: now,
                providers: want
                    .iter()
                    .map(|p| unknown(*p, UsageProblem::FormatChanged))
                    .collect(),
            });
            publish(snapshot.clone());
            return snapshot;
        }
        let cursor_was_due = want.contains(&Provider::Cursor)
            && cursor_due(&self.config, self.last_cursor_fetch, now);
        let mut providers = Vec::new();
        for provider in want {
            if providers
                .iter()
                .any(|p: &ProviderUsage| p.provider == *provider)
            {
                continue;
            }
            let usage = match provider {
                Provider::Claude => claude::read(&self.config.claude_state_file, now),
                Provider::Codex => codex::read(&self.config.codex_home, now),
                Provider::Cursor => self.refresh_cursor_reading(now, &mut fetch),
            };
            providers.push(usage);
        }
        if !providers.iter().any(|p| p.provider == Provider::Cursor) {
            if let Some(cursor) = &self.cursor_result {
                providers.push(cursor.clone());
            }
        }
        let mut snapshot = UsageSnapshot {
            version: 1,
            taken_at: now,
            providers,
        };
        self.save_snapshot(&mut snapshot);
        publish(snapshot.clone());
        // Publish before the child can block. The CLI cannot spend the login lease.
        if allow_login && cursor_was_due && self.retry_cursor_login(now, &mut fetch, command) {
            if let Some(cursor) = &self.cursor_result {
                if let Some(row) = snapshot
                    .providers
                    .iter_mut()
                    .find(|p| p.provider == Provider::Cursor)
                {
                    *row = cursor.clone();
                }
            }
            self.save_snapshot(&mut snapshot);
            publish(snapshot.clone());
        }
        snapshot
    }
    fn save_snapshot(&self, snapshot: &mut UsageSnapshot) {
        for usage in &mut snapshot.providers {
            for window in &mut usage.windows {
                if window.left_pct.is_some()
                    && window.resets_at.is_some_and(|t| t <= snapshot.taken_at)
                {
                    window.left_pct = Some(100);
                }
            }
        }
        if let Err(error) = atomic_json(&self.config.snapshot_file, snapshot) {
            tracing::warn!(%error, "could not write usage snapshot");
        }
    }
    #[cfg(test)]
    fn refresh_cursor(
        &mut self,
        now: u64,
        fetch: impl FnMut(&str) -> ProviderUsage,
    ) -> ProviderUsage {
        self.refresh_cursor_reading(now, fetch)
    }
    #[cfg(test)]
    fn refresh_cursor_with_login(
        &mut self,
        now: u64,
        mut fetch: impl FnMut(&str) -> ProviderUsage,
        command: impl FnOnce(std::time::Duration) -> login::CursorLoginResult,
    ) -> ProviderUsage {
        let due = cursor_due(&self.config, self.last_cursor_fetch, now);
        let result = self.refresh_cursor_reading(now, &mut fetch);
        if due && self.retry_cursor_login(now, &mut fetch, command) {
            self.cursor_result.clone().expect("login result")
        } else {
            result
        }
    }
    fn refresh_cursor_reading(
        &mut self,
        now: u64,
        mut fetch: impl FnMut(&str) -> ProviderUsage,
    ) -> ProviderUsage {
        if cursor_due(&self.config, self.last_cursor_fetch, now) {
            self.last_cursor_fetch = Some(now);
            if let Err(error) = atomic_json(&cursor_fetch_file(&self.config), &now) {
                tracing::warn!(%error, "could not save Cursor fetch time");
            }
            let result = fetch(&self.config.cursor_keychain_service);
            self.store_cursor_result(result);
        }
        self.cursor_result
            .clone()
            .unwrap_or_else(|| unknown(Provider::Cursor, UsageProblem::NoData))
    }
    fn retry_cursor_login(
        &mut self,
        now: u64,
        mut fetch: impl FnMut(&str) -> ProviderUsage,
        command: impl FnOnce(std::time::Duration) -> login::CursorLoginResult,
    ) -> bool {
        if !self.cursor_result.as_ref().is_some_and(|result| {
            matches!(
                result.problem,
                Some(UsageProblem::NoLogin | UsageProblem::LoginExpired)
            )
        }) {
            return false;
        }
        let result = if self.cursor_login.no_consent {
            unknown(Provider::Cursor, UsageProblem::NoLogin)
        } else {
            match self.cursor_login.attempt(&self.config, now, command) {
                Some(login::CursorLoginResult::Exited(0)) => {
                    fetch(&self.config.cursor_keychain_service)
                }
                Some(login::CursorLoginResult::Exited(3)) => {
                    unknown(Provider::Cursor, UsageProblem::NoLogin)
                }
                Some(login::CursorLoginResult::Exited(4)) => {
                    unknown(Provider::Cursor, UsageProblem::LoginExpired)
                }
                _ => return false,
            }
        };
        self.store_cursor_result(result);
        true
    }
    fn store_cursor_result(&mut self, mut result: ProviderUsage) {
        if result.problem == Some(UsageProblem::Offline) {
            if let Some(login) = self.cursor_result.as_ref().filter(|old| {
                matches!(
                    old.problem,
                    Some(UsageProblem::LoginExpired | UsageProblem::NoLogin)
                )
            }) {
                result = login.clone();
            } else if let Some(old) = &self.cursor_good {
                result = old.clone();
                result.problem = Some(UsageProblem::Offline);
            }
        } else if matches!(
            result.problem,
            Some(UsageProblem::LoginExpired | UsageProblem::NoLogin)
        ) {
            // A rejected login invalidates percentages before the importer retry.
            self.cursor_good = None;
        } else if result.problem.is_none() {
            self.cursor_good = Some(result.clone());
        }
        self.cursor_result = Some(result);
    }
    pub fn take_alerts(&mut self, snapshot: &UsageSnapshot) -> Vec<(Provider, WindowKind)> {
        let mut out = Vec::new();
        for p in &snapshot.providers {
            if matches!(
                p.problem,
                Some(
                    UsageProblem::NoLogin
                        | UsageProblem::LoginExpired
                        | UsageProblem::FormatChanged
                        | UsageProblem::NoData
                )
            ) {
                continue;
            }
            for w in &p.windows {
                if !w.left_pct.is_some_and(|left| left <= 10)
                    || w.resets_at.is_some_and(|reset| reset <= snapshot.taken_at)
                {
                    continue;
                }
                let key = (p.provider, w.kind);
                let same = self
                    .alerts
                    .get(&key)
                    .is_some_and(|old| match (*old, w.resets_at) {
                        (Some(a), Some(b)) => a.abs_diff(b) < 1800,
                        (None, None) => true,
                        _ => false,
                    });
                if !same {
                    self.alerts.insert(key, w.resets_at);
                    out.push(key);
                }
            }
        }
        out
    }
}
pub(super) fn unknown(provider: Provider, problem: UsageProblem) -> ProviderUsage {
    let kinds: &[WindowKind] = if provider == Provider::Cursor {
        &[WindowKind::Monthly]
    } else {
        &[WindowKind::FiveHour, WindowKind::Weekly]
    };
    ProviderUsage {
        provider,
        windows: kinds
            .iter()
            .map(|kind| UsageWindow {
                kind: *kind,
                left_pct: None,
                resets_at: None,
                note: None,
            })
            .collect(),
        seen_at: None,
        source: match provider {
            Provider::Claude => "claude status line",
            Provider::Codex => "codex log",
            Provider::Cursor => "cursor usage-summary",
        }
        .into(),
        problem: Some(problem),
    }
}
pub(super) fn percent(value: &Value) -> Option<u8> {
    let left = 100.0 - value.as_f64()?;
    if !left.is_finite() {
        return None;
    }
    Some(left.floor().clamp(0.0, 100.0) as u8)
}
pub(super) fn window(
    value: Option<&Value>,
    kind: WindowKind,
    field: &str,
    now: u64,
) -> Result<UsageWindow, ()> {
    let mut w = UsageWindow {
        kind,
        left_pct: None,
        resets_at: None,
        note: None,
    };
    if let Some(v) = value {
        w.resets_at = Some(v.get("resets_at").and_then(Value::as_u64).ok_or(())?);
        w.left_pct = Some(percent(v.get(field).ok_or(())?).ok_or(())?);
        if w.resets_at.is_some_and(|t| t <= now) {
            w.left_pct = Some(100);
        }
    }
    Ok(w)
}
pub(super) fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    let file = fs::File::open(path)?;
    let mut data = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut data)?;
    if data.len() > 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "usage file too large",
        ));
    }
    serde_json::from_slice(&data).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}
pub(super) fn atomic_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    drop(file);
    fs::rename(temp, path)
}
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub(super) use crate::platform::usage::local_time;
pub(super) fn timestamp(s: &str) -> Option<u64> {
    // Strict UTC RFC3339 with optional fractional seconds, without OS date parsing.
    if !s.is_ascii() || s.len() < 20 || !s.ends_with('Z') {
        return None;
    }
    let bytes = s.as_bytes();
    for (i, expected) in [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':')] {
        if bytes[i] != expected {
            return None;
        }
    }
    let number = |a: usize, b: usize| {
        s[a..b]
            .bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| s[a..b].parse::<u64>().ok())
            .flatten()
    };
    let year = number(0, 4)?;
    let month = number(5, 7)?;
    let day = number(8, 10)?;
    let hour = number(11, 13)?;
    let min = number(14, 16)?;
    let sec = number(17, 19)?;
    let suffix = &s[19..s.len() - 1];
    if !suffix.is_empty()
        && (!suffix.starts_with('.')
            || suffix.len() == 1
            || !suffix[1..].bytes().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    if year < 1970 || !(1..=12).contains(&month) || hour > 23 || min > 59 || sec > 59 {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let months = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if day == 0 || day > months[(month - 1) as usize] {
        return None;
    }
    let y = year - 1;
    let days = (year - 1970) * 365 + (y / 4 - y / 100 + y / 400)
        - (1969 / 4 - 1969 / 100 + 1969 / 400)
        + months[..(month - 1) as usize].iter().sum::<u64>()
        + day
        - 1;
    Some(days * 86400 + hour * 3600 + min * 60 + sec)
}
#[cfg(test)]
mod tests;

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
mod login_refresh;
pub mod openrouter;
pub(crate) use login_refresh::saved as cursor_login_saved;
pub mod palette;
pub mod statusline;

pub(crate) const CURSOR_MIN_INTERVAL_SECS: u64 = 900;

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
    /// OpenRouter free requests and paid credit. Its own provider, never GPT.
    OpenRouter,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowKind {
    FiveHour,
    Weekly,
    Monthly,
    /// Cursor's Auto pool. Shares the monthly reset with `Named`.
    Auto,
    /// Cursor's named-model pool (Grok, Claude, GPT and others picked by name).
    Named,
    /// Guard-only dollar budget, never a provider percentage pool.
    ExtraSpend,
    /// OpenRouter free models: requests left for the current UTC day.
    Daily,
    /// OpenRouter paid credit as a share of total credits. Has no reset.
    Credit,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageProblem {
    NoData,
    NoLogin,
    LoginExpired,
    FormatChanged,
    Offline,
    /// The key works but this reading needs more access (credits without a Management key).
    PermissionDenied,
    /// The provider asked us to slow down. The last good reading stays and dims with age.
    RateLimited,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    pub kind: WindowKind,
    pub left_pct: Option<u8>,
    pub resets_at: Option<u64>,
    pub note: Option<String>,
    /// The reset time had passed when this reading was stored. The percent is
    /// the one the provider reported, not a filled-in 100.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reset: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorSpend {
    pub bonus_units: u64,
    pub on_demand_units: u64,
    pub total_units: u64,
    pub cycle_end: u64,
}
impl CursorSpend {
    pub fn left_pct(&self) -> Option<u8> {
        if self.total_units == 0 {
            return None;
        }
        let extra = self.bonus_units.saturating_add(self.on_demand_units);
        Some(
            (u128::from(self.total_units.saturating_sub(extra)) * 100
                / u128::from(self.total_units)) as u8,
        )
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderUsage {
    pub provider: Provider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_spend: Option<CursorSpend>,
    /// OpenRouter's two readings, each with its own age. Absent for other providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openrouter: Option<openrouter::OpenRouterDetail>,
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
    pub claude_statusline: bool,
    /// Also read `$CODEX_HOME` and `~/.codex` besides the configured home.
    #[serde(default)]
    pub codex_scan_all: bool,
    pub visibility_file: Option<PathBuf>,
    /// Codex home of an OpenRouter-backed Codex. Its panes are OpenRouter, not GPT.
    #[serde(default)]
    pub openrouter_codex_home: PathBuf,
    /// Owner-only file with the OpenRouter inference key. The key is never serialized.
    #[serde(default)]
    pub openrouter_key_file: Option<PathBuf>,
    /// Name of the environment variable holding the OpenRouter inference key.
    #[serde(default)]
    pub openrouter_key_env: Option<String>,
    /// Owner-only file with an OpenRouter Management key, for the credit balance.
    #[serde(default)]
    pub openrouter_management_key_file: Option<PathBuf>,
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
            claude_statusline: true,
            // Unit tests stay hermetic: no real homes, no `ps`.
            codex_scan_all: !cfg!(test),
            visibility_file: None,
            fixture: std::env::var_os("HERDR_USAGE_FIXTURE").map(PathBuf::from),
            openrouter_codex_home: home.join(".codex-openrouter"),
            openrouter_key_file: None,
            openrouter_key_env: None,
            openrouter_management_key_file: None,
        }
    }
}
impl UsageConfig {
    /// The configured home first, then `$CODEX_HOME`, `~/.codex`, and the
    /// homes of Codex processes running in panes (`active`).
    pub fn codex_homes(&self, active: Vec<PathBuf>) -> Vec<codex::Home> {
        let mut homes = vec![codex::Home {
            path: self.codex_home.clone(),
            active: false,
        }];
        let mut add = |path: PathBuf, active: bool| {
            if let Some(known) = homes.iter_mut().find(|h| h.path == path) {
                known.active |= active;
            } else {
                homes.push(codex::Home { path, active });
            }
        };
        for path in active {
            add(path, true);
        }
        if !self.codex_scan_all {
            return homes;
        }
        if let Some(env) = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
            add(PathBuf::from(env), false);
        }
        if let Some(home) = std::env::var_os("HOME") {
            add(PathBuf::from(home).join(".codex"), false);
        }
        homes
    }

    /// True when an OpenRouter key source is configured. Without one nothing is requested.
    pub fn openrouter_configured(&self) -> bool {
        self.openrouter_key_file.is_some()
            || self.openrouter_key_env.is_some()
            || self.openrouter_management_key_file.is_some()
    }

    pub fn configured() -> Self {
        let mut config = Self::default();
        let display = crate::config::Config::load().config.ui.usage;
        config.claude_statusline = display.claude_statusline;
        config.visibility_file = Some(statusline::visibility_file());
        if let Some(path) = display.codex_home {
            config.codex_home = expand_home(&path);
        }
        if let Some(path) = display
            .openrouter_codex_home
            .filter(|p| !p.trim().is_empty())
        {
            config.openrouter_codex_home = expand_home(&path);
        }
        config.openrouter_key_file = display
            .openrouter_key_file
            .filter(|p| !p.trim().is_empty())
            .map(|p| expand_home(&p));
        config.openrouter_key_env = display
            .openrouter_key_env
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty());
        config.openrouter_management_key_file = display
            .openrouter_management_key_file
            .filter(|p| !p.trim().is_empty())
            .map(|p| expand_home(&p));
        config
    }
}

/// A configured path with a leading `~/` made absolute from `$HOME`.
fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(path),
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageState {
    config: UsageConfig,
    last_cursor_fetch: Option<u64>,
    #[serde(default)]
    cursor_login_revision: Option<String>,
    cursor_result: Option<ProviderUsage>,
    cursor_good: Option<ProviderUsage>,
    cursor_login: login::RefreshState,
    alerts: HashMap<(Provider, WindowKind), Option<u64>>,
    /// When the last OpenRouter request started. Memory only, so a restart polls at once.
    #[serde(skip)]
    openrouter_last_fetch: Option<u64>,
    /// The last merged OpenRouter row. Restored from the snapshot at start.
    #[serde(default)]
    openrouter_result: Option<ProviderUsage>,
    /// Set only by the app worker and the CLI. The default state never reaches the network.
    #[serde(skip)]
    openrouter_live: bool,
    /// Test-only OpenRouter answers, one per due poll.
    #[cfg(test)]
    #[serde(skip)]
    openrouter_fixtures: std::collections::VecDeque<openrouter::Fetched>,
    /// Shell pids of panes showing a Codex agent, set by the app worker.
    #[serde(skip)]
    codex_pane_pids: Vec<u32>,
    /// `CODEX_HOME` of each Codex process already looked at, by its pid.
    #[serde(skip)]
    codex_pid_homes: HashMap<u32, PathBuf>,
    /// The Codex home of each Codex pane's shell, from the latest resolution.
    #[serde(skip)]
    codex_shell_homes: Vec<(u32, PathBuf)>,
}
impl UsageState {
    /// The shell pids of the panes that run Codex. The worker resolves each to
    /// its Codex process and reads that process's `CODEX_HOME`.
    pub(crate) fn set_codex_panes(&mut self, shell_pids: Vec<u32>) {
        self.codex_pane_pids = shell_pids;
    }
    /// App worker: the shell pids of Codex panes whose process runs with the
    /// OpenRouter home. Those panes are OpenRouter, not GPT.
    pub(crate) fn resolve_openrouter_panes(&mut self) -> Vec<u32> {
        self.load_config_paths();
        let _ = self.codex_pane_homes();
        let home = &self.config.openrouter_codex_home;
        self.codex_shell_homes
            .iter()
            .filter(|(_, path)| path == home)
            .map(|(pid, _)| *pid)
            .collect()
    }
    /// Copies the paths that come from the configuration. Called on every refresh.
    fn load_config_paths(&mut self) {
        let configured = UsageConfig::configured();
        self.config.codex_home = configured.codex_home;
        self.config.openrouter_codex_home = configured.openrouter_codex_home;
        self.config.openrouter_key_file = configured.openrouter_key_file;
        self.config.openrouter_key_env = configured.openrouter_key_env;
        self.config.openrouter_management_key_file = configured.openrouter_management_key_file;
    }
    /// Homes of Codex processes running in panes. The environment is read once
    /// per Codex pid (one sysctl on macOS, one file read on Linux).
    fn codex_pane_homes(&mut self) -> Vec<PathBuf> {
        let user = std::env::var_os("HOME").map(PathBuf::from);
        let mut live = Vec::new();
        let mut homes = Vec::new();
        self.codex_shell_homes.clear();
        for shell in &self.codex_pane_pids {
            let Some(job) = crate::detect::foreground_job(*shell) else {
                continue;
            };
            if !matches!(
                crate::detect::identify_agent_in_job(&job),
                Some((crate::detect::Agent::Codex, _))
            ) {
                continue;
            }
            // The codex process itself: a `codex-gpt` wrapper execs it, so it
            // carries the variable. A node launcher is matched by its argv.
            let names_codex = |s: &str| s.rsplit('/').next() == Some("codex");
            let Some(process) = job.processes.iter().find(|p| {
                names_codex(&p.name)
                    || p.argv0.as_deref().is_some_and(names_codex)
                    || p.argv
                        .as_ref()
                        .is_some_and(|argv| argv.iter().any(|a| names_codex(a)))
            }) else {
                continue;
            };
            live.push(process.pid);
            let home = self.codex_pid_homes.entry(process.pid).or_insert_with(|| {
                crate::platform::process_env_var(process.pid, "CODEX_HOME")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
                    // No CODEX_HOME means the default home.
                    .or_else(|| user.as_ref().map(|u| u.join(".codex")))
                    .unwrap_or_default()
            });
            if !home.as_os_str().is_empty() {
                self.codex_shell_homes.push((*shell, home.clone()));
                if !homes.contains(&*home) {
                    homes.push(home.clone());
                }
            }
        }
        self.codex_pid_homes.retain(|pid, _| live.contains(pid));
        homes
    }
}

/// How long after `resets_at` to wake, so the check is not early.
pub(crate) const RESET_WAKE_MARGIN_SECS: u64 = 2;

/// One recovery notice for a window that was low and is back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AllowanceRecovery {
    pub(crate) provider: Provider,
    pub(crate) kind: WindowKind,
    /// Remaining allowance after a climb. Unused when `reset` is set.
    pub(crate) left_pct: u8,
    /// The reset time passed. The spoken line must not invent a percentage.
    pub(crate) reset: bool,
}

impl UsageState {
    pub fn new(config: UsageConfig) -> Self {
        // Read the revision first so a concurrent login is noticed on refresh.
        let cursor_login_revision = login_refresh::revision(&config);
        let cached = read_json::<UsageSnapshot>(&config.snapshot_file).ok();
        let cursor_result = cached
            .as_ref()
            .and_then(|s| s.providers.iter().find(|p| p.provider == Provider::Cursor))
            .cloned();
        let openrouter_result = cached
            .as_ref()
            .and_then(|s| {
                s.providers
                    .iter()
                    .find(|p| p.provider == Provider::OpenRouter)
            })
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
            cursor_login_revision,
            cursor_result,
            cursor_good,
            alerts: HashMap::new(),
            openrouter_last_fetch: None,
            openrouter_result,
            openrouter_live: false,
            #[cfg(test)]
            openrouter_fixtures: std::collections::VecDeque::new(),
            codex_pane_pids: Vec::new(),
            codex_pid_homes: HashMap::new(),
            codex_shell_homes: Vec::new(),
        }
    }
    /// Fetch readings without importing a login or spending its hourly attempt.
    /// OpenRouter is not requested here: this path never touches the network.
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
        self.load_config_paths();
        self.openrouter_live = true;
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
        self.sync_saved_cursor_login();
        let cursor_was_due = want.contains(&Provider::Cursor)
            && cursor_due(&self.config, self.last_cursor_fetch, now);
        // One pass over the Codex panes serves both the Codex and the OpenRouter rows.
        let active = if want
            .iter()
            .any(|p| matches!(p, Provider::Codex | Provider::OpenRouter))
        {
            self.codex_pane_homes()
        } else {
            Vec::new()
        };
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
                Provider::Codex => {
                    // Panes running with the OpenRouter home are OpenRouter, never GPT.
                    let homes: Vec<codex::Home> = self
                        .config
                        .codex_homes(active.clone())
                        .into_iter()
                        .filter(|h| h.path != self.config.openrouter_codex_home)
                        .collect();
                    codex::read_homes(&homes, now)
                }
                Provider::Cursor => self.refresh_cursor_reading(now, &mut fetch),
                Provider::OpenRouter => self.refresh_openrouter(now),
            };
            providers.push(usage);
        }
        if !providers.iter().any(|p| p.provider == Provider::Cursor) {
            if let Some(cursor) = &self.cursor_result {
                providers.push(cursor.clone());
            }
        }
        // OpenRouter is not polled this round: keep the shared reading, so the snapshot write
        // does not erase it. Its windows are recomputed for the current UTC day.
        if !providers.iter().any(|p| p.provider == Provider::OpenRouter) {
            if let Some(row) = self.openrouter_result.as_mut() {
                if let Some(detail) = row.openrouter.as_ref() {
                    row.windows = openrouter::windows(detail, now);
                }
                providers.push(row.clone());
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
    /// The OpenRouter row. A request starts only when the poll interval has passed,
    /// so at most one per interval. Between polls the merged row is returned as is.
    fn refresh_openrouter(&mut self, now: u64) -> ProviderUsage {
        let due = self
            .openrouter_last_fetch
            .is_none_or(|last| now.saturating_sub(last) >= openrouter::POLL_MIN_INTERVAL_SECS);
        if due {
            self.openrouter_last_fetch = Some(now);
            let fetched = self.fetch_openrouter(now);
            self.store_openrouter(fetched, now);
        }
        self.openrouter_result
            .clone()
            .unwrap_or_else(openrouter::placeholder)
    }
    /// Live network only in the worker and the CLI. Tests take their answers from the fixture queue.
    fn fetch_openrouter(&mut self, now: u64) -> openrouter::Fetched {
        #[cfg(test)]
        {
            if let Some(next) = self.openrouter_fixtures.pop_front() {
                return next;
            }
        }
        if self.openrouter_live {
            openrouter::fetch_live(&self.config, now)
        } else {
            openrouter::Fetched::none()
        }
    }
    /// Merges one poll into the last row. A good reading replaces its part. A rate limit or an
    /// offline poll keeps the last good free count and credit balance, which then dim with age.
    /// Any other failure clears that part: an old number never stands in for a refused one.
    fn store_openrouter(&mut self, fetched: openrouter::Fetched, now: u64) {
        let mut detail = self
            .openrouter_result
            .as_ref()
            .and_then(|row| row.openrouter.clone())
            .unwrap_or_default();
        match fetched.free {
            Ok(free) => {
                detail.free = Some(free);
                detail.free_problem = None;
            }
            Err(problem) => {
                if !matches!(problem, UsageProblem::Offline | UsageProblem::RateLimited) {
                    detail.free = None;
                }
                detail.free_problem = Some(problem);
            }
        }
        match fetched.credit {
            Ok(credit) => {
                detail.credit = Some(credit);
                detail.credit_problem = None;
            }
            Err(problem) => {
                if !matches!(problem, UsageProblem::Offline | UsageProblem::RateLimited) {
                    detail.credit = None;
                }
                detail.credit_problem = Some(problem);
            }
        }
        let windows = openrouter::windows(&detail, now);
        let seen_at = [
            detail.free.as_ref().map(|free| free.seen_at),
            detail.credit.as_ref().map(|credit| credit.seen_at),
        ]
        .into_iter()
        .flatten()
        .max();
        let problem = if detail.free.is_some() || detail.credit.is_some() {
            None
        } else {
            detail
                .free_problem
                .clone()
                .or_else(|| detail.credit_problem.clone())
        };
        self.openrouter_result = Some(ProviderUsage {
            provider: Provider::OpenRouter,
            extra_spend: None,
            openrouter: Some(detail),
            windows,
            seen_at,
            source: "openrouter api".into(),
            problem,
        });
    }
    fn sync_saved_cursor_login(&mut self) {
        let revision = login_refresh::revision(&self.config);
        if revision.is_none() || revision == self.cursor_login_revision {
            return;
        }
        self.cursor_login_revision = revision;
        self.last_cursor_fetch = None;
        self.cursor_good = None;
        if let Some(cursor) = &mut self.cursor_result {
            if matches!(
                cursor.problem,
                Some(UsageProblem::LoginExpired | UsageProblem::NoLogin)
            ) {
                *cursor = unknown(Provider::Cursor, UsageProblem::NoData);
            }
        }
    }
    fn save_snapshot(&self, snapshot: &mut UsageSnapshot) {
        let _lock = match crate::platform::usage::lock_statusline(&self.config.snapshot_file) {
            Ok(lock) => lock,
            Err(error) => {
                tracing::warn!(%error, "could not lock usage snapshot");
                return;
            }
        };
        // An old request may finish after the login command has cleared the cache.
        // Do not publish its obsolete auth error over the pending fresh reading.
        if login_refresh::revision(&self.config) != self.cursor_login_revision {
            login_refresh::clear_auth_problem(snapshot);
        }
        for usage in &mut snapshot.providers {
            for window in &mut usage.windows {
                if window.left_pct.is_some()
                    && window.resets_at.is_some_and(|t| t <= snapshot.taken_at)
                {
                    window.reset = true;
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
                    // The in-process importer also publishes a login revision. Its
                    // immediate fetch consumes that reset and restores the poll timer.
                    self.sync_saved_cursor_login();
                    self.last_cursor_fetch = None;
                    self.refresh_cursor_reading(now, &mut fetch)
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
                // Credit is a dollar balance with no reset. It never speaks as a percentage alert.
                if w.kind == WindowKind::Credit {
                    continue;
                }
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

    /// A window that was already spoken for, because it was at or under 10%
    /// left, is back. Fires once when its reset time has passed or its
    /// remaining allowance is above 10% again. A window that was never low is
    /// not in the map, so startup does not speak. A climb keeps the provider's
    /// remaining percent. A passed reset stores 100, matching the full bar,
    /// and is marked `reset` so the spoken line does not say a percentage.
    pub(crate) fn take_recoveries(
        &mut self,
        snapshot: &UsageSnapshot,
        now: u64,
    ) -> Vec<AllowanceRecovery> {
        let keys: Vec<_> = self.alerts.keys().copied().collect();
        let mut out = Vec::new();
        for key in keys {
            let Some(usage) = snapshot.providers.iter().find(|p| p.provider == key.0) else {
                continue;
            };
            if provider_blocks_recovery(usage) {
                continue;
            }
            let Some(window) = usage.windows.iter().find(|w| w.kind == key.1) else {
                continue;
            };
            let reset_passed = window.resets_at.is_some_and(|at| at <= now);
            let rose = window.left_pct.is_some_and(|left| left > 10);
            if !(reset_passed || rose) {
                continue;
            }
            self.alerts.remove(&key);
            out.push(AllowanceRecovery {
                provider: key.0,
                kind: key.1,
                left_pct: if reset_passed {
                    100
                } else {
                    window.left_pct.unwrap_or(100)
                },
                reset: reset_passed,
            });
        }
        out
    }

    /// Seconds until a low window should wake: its reset time plus a short
    /// margin, so the wake is not early. `Some(0)` when that moment has passed.
    /// A provider problem that `take_recoveries` skips does not wake, so a
    /// passed reset does not return 0 on every call.
    pub(crate) fn recovery_wake_secs(&self, snapshot: &UsageSnapshot, now: u64) -> Option<u64> {
        let mut next: Option<u64> = None;
        for &(provider, kind) in self.alerts.keys() {
            let Some(usage) = snapshot.providers.iter().find(|p| p.provider == provider) else {
                continue;
            };
            if provider_blocks_recovery(usage) {
                continue;
            }
            let Some(window) = usage.windows.iter().find(|w| w.kind == kind) else {
                continue;
            };
            let Some(at) = window.resets_at else {
                continue;
            };
            let secs = at
                .saturating_add(RESET_WAKE_MARGIN_SECS)
                .saturating_sub(now);
            next = Some(next.map_or(secs, |cur| cur.min(secs)));
        }
        next
    }
}
fn provider_blocks_recovery(usage: &ProviderUsage) -> bool {
    matches!(
        usage.problem,
        Some(
            UsageProblem::NoLogin
                | UsageProblem::LoginExpired
                | UsageProblem::FormatChanged
                | UsageProblem::NoData
        )
    )
}
pub(super) fn unknown(provider: Provider, problem: UsageProblem) -> ProviderUsage {
    let kinds: &[WindowKind] = match provider {
        Provider::Cursor => &[WindowKind::Auto, WindowKind::Named],
        Provider::OpenRouter => &[WindowKind::Daily, WindowKind::Credit],
        Provider::Claude | Provider::Codex => &[WindowKind::FiveHour, WindowKind::Weekly],
    };
    ProviderUsage {
        extra_spend: None,
        openrouter: None,
        provider,
        windows: kinds
            .iter()
            .map(|kind| UsageWindow {
                kind: *kind,
                left_pct: None,
                resets_at: None,
                note: None,
                reset: false,
            })
            .collect(),
        seen_at: None,
        source: match provider {
            Provider::Claude => "claude status line",
            Provider::Codex => "codex log",
            Provider::Cursor => "cursor usage-summary",
            Provider::OpenRouter => "openrouter api",
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
        reset: false,
    };
    if let Some(v) = value {
        w.resets_at = Some(v.get("resets_at").and_then(Value::as_u64).ok_or(())?);
        w.left_pct = Some(percent(v.get(field).ok_or(())?).ok_or(())?);
        if w.resets_at.is_some_and(|t| t <= now) {
            w.reset = true;
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

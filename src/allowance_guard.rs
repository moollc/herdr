//! The allowance guard: when a provider's five hour allowance runs low, or
//! falls fast, every working agent on that provider is told to wind down, and
//! at the stop line is interrupted. This file holds the settings and the
//! decisions. It reads no clock and touches no pane, so it is tested with fake
//! readings. `app::allowance_guard` carries the decisions out.

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::terminal::TerminalId;
use crate::usage::{Provider, ProviderUsage, UsageProblem, WindowKind};

/// A provider re-arms once its allowance is back above this.
#[cfg(test)]
const REARM_ABOVE_PCT: u8 = 50;

/// Cached percentages cannot authorize a note or interrupt indefinitely.
pub(crate) const MAX_READING_AGE_SECS: u64 = 15 * 60;

/// `[allowance_guard]`. Percentages are what is left of the five hour window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardConfig {
    /// At or below this, every working pane on the provider gets one wind down note.
    pub wind_down_pct: u8,
    /// A fall of this many points within `fast_window_minutes` does the same.
    pub fast_drop_points: u8,
    pub fast_window_minutes: u64,
    /// At or below this, working panes on the provider are interrupted.
    pub stop_pct: u8,
    pub claude: bool,
    pub codex: bool,
    /// Off by default: the Cursor Auto pool is run to zero on purpose.
    pub cursor: bool,
    /// Provider overrides. Legacy scalar fields remain accepted.
    pub providers: HashMap<Provider, ProviderLimits>,
    /// Bounded, terminal-specific exceptions; provider policy stays enabled.
    pub terminal_exceptions: HashMap<String, TerminalException>,
    legacy_thresholds: bool,
}

impl Default for GuardConfig {
    fn default() -> Self {
        Self {
            wind_down_pct: 15,
            fast_drop_points: 25,
            fast_window_minutes: 10,
            stop_pct: 3,
            claude: true,
            codex: true,
            cursor: false,
            providers: HashMap::new(),
            terminal_exceptions: HashMap::new(),
            legacy_thresholds: false,
        }
    }
}

// Remember explicit legacy scalars, including 15, so old configurations retain their limits.
impl<'de> Deserialize<'de> for GuardConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct Stored {
            wind_down_pct: Option<u8>,
            stop_pct: Option<u8>,
            fast_drop_points: Option<u8>,
            fast_window_minutes: Option<u64>,
            claude: Option<bool>,
            codex: Option<bool>,
            cursor: Option<bool>,
            providers: HashMap<Provider, ProviderLimits>,
            terminal_exceptions: HashMap<String, TerminalException>,
        }
        let stored = Stored::deserialize(deserializer)?;
        let mut result = Self {
            legacy_thresholds: stored.wind_down_pct.is_some()
                || stored.stop_pct.is_some()
                || stored.fast_drop_points.is_some()
                || stored.fast_window_minutes.is_some(),
            ..Self::default()
        };
        if let Some(v) = stored.wind_down_pct {
            result.wind_down_pct = v;
        }
        if let Some(v) = stored.stop_pct {
            result.stop_pct = v;
        }
        if let Some(v) = stored.fast_drop_points {
            result.fast_drop_points = v;
        }
        if let Some(v) = stored.fast_window_minutes {
            result.fast_window_minutes = v;
        }
        if let Some(v) = stored.claude {
            result.claude = v;
        }
        if let Some(v) = stored.codex {
            result.codex = v;
        }
        if let Some(v) = stored.cursor {
            result.cursor = v;
        }
        result.providers = stored.providers;
        result.terminal_exceptions = stored.terminal_exceptions;
        Ok(result)
    }
}

/// An explicit temporary allowance override for one terminal and provider.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TerminalException {
    pub provider: Provider,
    pub starts_at: u64,
    pub expires_at: u64,
    pub minimum_remaining_pct: u8,
}

impl TerminalException {
    fn permits(&self, provider: Provider, now: u64, pct: u8) -> bool {
        self.provider == provider
            && self.starts_at <= now
            && now < self.expires_at
            && self.expires_at.saturating_sub(self.starts_at) <= 3600
            && self.minimum_remaining_pct >= 1
            && pct > self.minimum_remaining_pct
    }
}

/// Separate limits for a short window and a long window (Cursor Auto/Named).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowLimits {
    pub wind_down_pct: u8,
    pub stop_pct: u8,
    pub overseer_pct: u8,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpendLimits {
    pub enabled: bool,
    #[serde(flatten)]
    pub limits: WindowLimits,
}
impl Default for SpendLimits {
    fn default() -> Self {
        Self {
            enabled: false,
            limits: WindowLimits {
                wind_down_pct: 15,
                overseer_pct: 8,
                stop_pct: 3,
            },
        }
    }
}
impl SpendLimits {
    fn window(&self, usage: &ProviderUsage) -> Option<crate::usage::UsageWindow> {
        if !self.enabled {
            return None;
        }
        let current = usage.extra_spend.as_ref()?;
        Some(crate::usage::UsageWindow {
            kind: WindowKind::ExtraSpend,
            left_pct: current.left_pct(),
            resets_at: Some(current.cycle_end),
            note: Some("bonus + on-demand, percent left of reported total".into()),
            reset: false,
        })
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderLimits {
    pub enabled: bool,
    pub short: WindowLimits,
    pub long: WindowLimits,
    pub fast_drop_points: u8,
    pub fast_window_minutes: u64,
    #[serde(default)]
    pub extra_spend: SpendLimits,
}
impl ProviderLimits {
    pub fn validate(&self) -> Result<(), &'static str> {
        let mut windows = vec![&self.short, &self.long];
        if self.extra_spend.enabled {
            windows.push(&self.extra_spend.limits);
        }
        for w in windows {
            if w.wind_down_pct > 100 || w.stop_pct >= w.wind_down_pct {
                return Err("Stop at must be below Wind-down at (0–100).");
            }
            if w.overseer_pct > w.wind_down_pct || w.overseer_pct <= w.stop_pct {
                return Err(
                    "Overseer reserve must be above Stop at and no higher than Wind-down at.",
                );
            }
        }
        if self.fast_drop_points == 0
            || self.fast_drop_points > 100
            || self.fast_window_minutes == 0
            || self.fast_window_minutes > 1440
        {
            return Err("Fast fall needs 1–100 points and 1–1440 minutes.");
        }
        Ok(())
    }
    pub fn window(&self, kind: WindowKind) -> &WindowLimits {
        match kind {
            WindowKind::ExtraSpend => &self.extra_spend.limits,
            WindowKind::Weekly | WindowKind::Monthly | WindowKind::Named => &self.long,
            _ => &self.short,
        }
    }
}
impl GuardConfig {
    pub fn effective(&self, provider: Provider) -> ProviderLimits {
        let wind =
            if provider == Provider::Codex && self.wind_down_pct == 15 && !self.legacy_thresholds {
                5
            } else {
                self.wind_down_pct
            };
        let short = WindowLimits {
            wind_down_pct: wind,
            stop_pct: self.stop_pct,
            overseer_pct: wind.saturating_sub(1).max(self.stop_pct.saturating_add(1)),
        };
        let long = WindowLimits {
            wind_down_pct: self.wind_down_pct,
            stop_pct: self.stop_pct,
            overseer_pct: 8
                .min(self.wind_down_pct.saturating_sub(1))
                .max(self.stop_pct.saturating_add(1)),
        };
        let enabled = match provider {
            Provider::Claude => self.claude,
            Provider::Codex => self.codex,
            Provider::Cursor => self.cursor,
            // OpenRouter has no percentage pool to guard: its free count and dollar balance are not a limit.
            Provider::OpenRouter => false,
        };
        let defaults = ProviderLimits {
            enabled,
            short,
            long,
            fast_drop_points: self.fast_drop_points,
            fast_window_minutes: self.fast_window_minutes,
            extra_spend: SpendLimits::default(),
        };
        self.providers
            .get(&provider)
            .filter(|p| p.validate().is_ok())
            .cloned()
            .map(|mut p| {
                p.enabled &= enabled;
                p
            })
            .unwrap_or(defaults)
    }
    pub fn watches(&self, provider: Provider) -> bool {
        self.effective(provider).enabled
    }
    pub fn policy(&self) -> HashMap<Provider, ProviderLimits> {
        [Provider::Claude, Provider::Codex, Provider::Cursor]
            .into_iter()
            .map(|p| (p, self.effective(p)))
            .collect()
    }
}

/// Known observation for one window; an elapsed reset is an explicit estimate.
/// Observation timestamps are preserved so fresh data always replaces the estimate.
pub fn window_reading(
    usage: &ProviderUsage,
    w: &crate::usage::UsageWindow,
    now: u64,
    working: bool,
) -> Option<u8> {
    if matches!(
        usage.problem,
        Some(
            UsageProblem::NoLogin
                | UsageProblem::LoginExpired
                | UsageProblem::FormatChanged
                | UsageProblem::NoData
        )
    ) {
        return None;
    }
    let seen = usage.seen_at?;
    if seen > now {
        return None;
    }
    if w.resets_at.is_some_and(|at| at <= now && seen <= at) && w.left_pct.is_some() {
        return Some(100);
    }
    if (!working && now.saturating_sub(seen) > MAX_READING_AGE_SECS)
        || w.reset
        || (w.kind == WindowKind::FiveHour
            && w.resets_at
                .is_some_and(|at| seen < at.saturating_sub(5 * 3600)))
    {
        return None;
    }
    w.left_pct.map(|p| p.min(100))
}

/// What a new reading asks the app to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    WindDown {
        provider: Provider,
        pct: u8,
        fast: bool,
    },
    Stop {
        provider: Provider,
        pct: u8,
    },
}

/// One provider while the guard is active on it.
#[derive(Debug, Default)]
struct Phase {
    fast: bool,
    stopped: bool,
    kind: Option<WindowKind>,
    reset: Option<u64>,
    pct: u8,
    /// Panes already given the note, and panes already interrupted, this window.
    noted: HashSet<TerminalId>,
    interrupted: HashSet<TerminalId>,
}

#[derive(Debug, Default)]
pub struct Guard {
    pub config: GuardConfig,
    #[cfg(test)]
    history: HashMap<Provider, VecDeque<(u64, u8)>>,
    #[cfg(test)]
    last: HashMap<Provider, u8>,
    phases: HashMap<Provider, Phase>,
    reserves: HashMap<Provider, Phase>,
    window_history: HashMap<(Provider, WindowKind), VecDeque<(u64, u8)>>,
}

impl Guard {
    pub fn new(config: GuardConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    /// Evaluate both windows and both roles. Each window has its own fast-fall history.
    pub fn observe_usage(&mut self, usage: &ProviderUsage, now: u64, working: bool) -> Vec<Event> {
        let provider = usage.provider;
        let policy = self.config.effective(provider);
        if !policy.enabled {
            self.phases.remove(&provider);
            self.reserves.remove(&provider);
            self.window_history.retain(|(p, _), _| *p != provider);
            return Vec::new();
        }
        let spend_window = policy.extra_spend.window(usage).filter(|_| {
            usage
                .seen_at
                .is_some_and(|seen| seen <= now && now - seen <= MAX_READING_AGE_SECS)
        });
        let mut readings = Vec::new();
        for w in usage.windows.iter().chain(spend_window.iter()) {
            let Some(pct) = window_reading(usage, w, now, working) else {
                continue;
            };
            let h = self.window_history.entry((provider, w.kind)).or_default();
            if w.resets_at.is_some_and(|at| at <= now) || h.back().is_some_and(|(_, p)| pct > *p) {
                h.clear();
            }
            let seen = usage.seen_at.unwrap_or(now);
            if h.back().is_none_or(|(at, _)| seen > *at) {
                h.push_back((seen, pct));
            }
            while h.len() > 64
                || h.front().is_some_and(|(at, _)| {
                    now.saturating_sub(*at) > policy.fast_window_minutes.saturating_mul(60)
                })
            {
                h.pop_front();
            }
            let fast = w.kind != WindowKind::ExtraSpend
                && h.iter()
                    .map(|(_, p)| *p)
                    .max()
                    .unwrap_or(pct)
                    .saturating_sub(pct)
                    >= policy.fast_drop_points;
            readings.push((w, pct, fast));
        }
        let mut events = Vec::new();
        for overseer in [false, true] {
            let phases = if overseer {
                &mut self.reserves
            } else {
                &mut self.phases
            };
            // A missing reading suspends action. It does not invent recovery.
            if readings.is_empty() {
                continue;
            }
            if phases.get(&provider).is_some_and(|phase| {
                readings.iter().any(|(w, pct, _)| {
                    Some(w.kind) == phase.kind
                        && ((*pct > 50 && *pct > phase.pct)
                            || phase.reset.is_some_and(|at| at <= now)
                            || w.resets_at.is_some_and(|at| at <= now))
                })
            }) {
                phases.remove(&provider);
            }
            let trigger = readings
                .iter()
                .filter(|(w, pct, fast)| {
                    let limits = policy.window(w.kind);
                    *pct <= if overseer {
                        limits.overseer_pct
                    } else {
                        limits.wind_down_pct
                    } || (!overseer && *fast)
                })
                .min_by_key(|(w, pct, _)| {
                    (
                        *pct > policy.window(w.kind).stop_pct,
                        i16::from(*pct)
                            - i16::from(if overseer {
                                policy.window(w.kind).overseer_pct
                            } else {
                                policy.window(w.kind).wind_down_pct
                            }),
                    )
                });
            if let Some((w, pct, fast)) = trigger {
                let fresh = !phases.contains_key(&provider);
                let phase = phases.entry(provider).or_default();
                phase.kind = Some(w.kind);
                phase.reset = w.resets_at;
                phase.pct = *pct;
                phase.fast = *fast;
                if fresh && !overseer {
                    events.push(Event::WindDown {
                        provider,
                        pct: *pct,
                        fast: *fast,
                    });
                }
                if *pct <= policy.window(w.kind).stop_pct && !phase.stopped {
                    phase.stopped = true;
                    if !overseer {
                        events.push(Event::Stop {
                            provider,
                            pct: *pct,
                        });
                    }
                }
            }
        }
        events
    }
    pub fn reset_provider(&mut self, provider: Provider) {
        self.phases.remove(&provider);
        self.reserves.remove(&provider);
        self.window_history.retain(|(p, _), _| *p != provider);
    }
    pub fn policy_kind(&self, provider: Provider) -> WindowKind {
        self.phases
            .get(&provider)
            .and_then(|p| p.kind)
            .unwrap_or(WindowKind::FiveHour)
    }
    pub fn pane_action(
        &self,
        provider: Provider,
        overseer: bool,
        pane: &TerminalId,
        usage: &ProviderUsage,
        context: (u64, bool),
    ) -> Option<(WindowKind, u8, bool, bool, bool)> {
        if !self.config.watches(provider) {
            return None;
        }
        let phase = if overseer {
            self.reserves.get(&provider)
        } else {
            self.phases.get(&provider)
        }?;
        let kind = phase.kind?;
        let policy = self.config.effective(provider);
        let (now, working) = context;
        let spend_window = policy.extra_spend.window(usage).filter(|_| {
            usage
                .seen_at
                .is_some_and(|seen| seen <= now && now - seen <= MAX_READING_AGE_SECS)
        });
        let window = usage
            .windows
            .iter()
            .chain(spend_window.iter())
            .find(|w| w.kind == kind)?;
        let pct = window_reading(usage, window, now, working)?;
        if window.resets_at.is_some_and(|at| at <= now) {
            return None;
        }
        if !phase.fast
            && self
                .config
                .terminal_exceptions
                .get(&pane.to_string())
                .is_some_and(|exception| exception.permits(provider, now, pct))
        {
            return None;
        }
        Some((
            kind,
            pct,
            phase.fast,
            !phase.noted.contains(pane),
            phase.stopped && !phase.interrupted.contains(pane),
        ))
    }
    pub fn mark_policy_action(
        &mut self,
        provider: Provider,
        overseer: bool,
        pane: &TerminalId,
        interrupted: bool,
    ) {
        let phase = if overseer {
            self.reserves.get_mut(&provider)
        } else {
            self.phases.get_mut(&provider)
        };
        if let Some(phase) = phase {
            if interrupted {
                phase.interrupted.insert(pane.clone());
            } else {
                phase.noted.insert(pane.clone());
            }
        }
    }

    /// True while the guard is active on the provider, for the usage bar mark.
    pub fn active(&self, provider: Provider) -> bool {
        self.phases.contains_key(&provider)
    }

    pub fn any_active(&self) -> bool {
        !self.phases.is_empty() || !self.reserves.is_empty()
    }

    /// True once per pane and window: the pane has not had the note yet.
    #[cfg(test)]
    pub fn needs_note(&self, provider: Provider, pane: &TerminalId) -> bool {
        self.phases
            .get(&provider)
            .is_some_and(|p| !p.noted.contains(pane))
    }

    #[cfg(test)]
    pub fn mark_noted(&mut self, provider: Provider, pane: &TerminalId) {
        if let Some(phase) = self.phases.get_mut(&provider) {
            phase.noted.insert(pane.clone());
        }
    }

    #[cfg(test)]
    pub fn needs_interrupt(&self, provider: Provider, pane: &TerminalId) -> bool {
        self.phases
            .get(&provider)
            .is_some_and(|p| p.stopped && !p.interrupted.contains(pane))
    }

    #[cfg(test)]
    pub fn mark_interrupted(&mut self, provider: Provider, pane: &TerminalId) {
        if let Some(phase) = self.phases.get_mut(&provider) {
            phase.interrupted.insert(pane.clone());
        }
    }

    /// Forgets panes that have gone, so the sets do not grow.
    pub fn retain_panes(&mut self, live: impl Fn(&TerminalId) -> bool) {
        for phase in self.phases.values_mut().chain(self.reserves.values_mut()) {
            phase.noted.retain(|id| live(id));
            phase.interrupted.retain(|id| live(id));
        }
    }

    /// The percent the provider was last seen at.
    #[cfg(test)]
    pub fn last_pct(&self, provider: Provider) -> Option<u8> {
        self.last.get(&provider).copied()
    }

    /// A provider whose reading cannot be used (no login, stale, switched off)
    /// takes no new action. An active phase stays until the allowance is seen
    /// back above 50%, so a missing reading never re-arms it by accident.
    #[cfg(test)]
    pub fn observe(&mut self, provider: Provider, pct: u8, now: u64) -> Vec<Event> {
        if !self.config.watches(provider) {
            self.phases.remove(&provider);
            self.history.remove(&provider);
            return Vec::new();
        }
        self.last.insert(provider, pct);
        let window = self.config.fast_window_minutes.saturating_mul(60);
        let history = self.history.entry(provider).or_default();
        history.push_back((now, pct));
        while history.len() > 64 {
            history.pop_front();
        }
        while history
            .front()
            .is_some_and(|(at, _)| now.saturating_sub(*at) > window)
        {
            history.pop_front();
        }
        let high = history.iter().map(|(_, p)| *p).max().unwrap_or(pct);
        let fast = high.saturating_sub(pct) >= self.config.fast_drop_points;
        if self.phases.contains_key(&provider) && pct > REARM_ABOVE_PCT {
            self.phases.remove(&provider);
            self.history.remove(&provider);
            return Vec::new();
        }
        let mut events = Vec::new();
        if !self.phases.contains_key(&provider) && (pct <= self.config.wind_down_pct || fast) {
            self.phases.insert(
                provider,
                Phase {
                    fast,
                    ..Phase::default()
                },
            );
            events.push(Event::WindDown {
                provider,
                pct,
                fast,
            });
        }
        if let Some(phase) = self.phases.get_mut(&provider) {
            if !phase.stopped && pct <= self.config.stop_pct {
                phase.stopped = true;
                events.push(Event::Stop { provider, pct });
            }
        }
        events
    }
}

/// The percent left that the guard acts on, or `None` when the reading cannot
/// be trusted: a login problem, no number, or an old reading. A window past
/// its reset is unknown until a new reading arrives. Claude and Codex use the five hour window; Cursor uses
/// the pool with less left. Age beyond fifteen minutes is unknown only while no
/// session on that provider is working; reset and invalid-source checks always apply.
#[cfg(test)]
pub fn reading(usage: &ProviderUsage, now: u64, provider_working: bool) -> Option<u8> {
    if matches!(
        usage.problem,
        Some(
            UsageProblem::NoLogin
                | UsageProblem::LoginExpired
                | UsageProblem::FormatChanged
                | UsageProblem::NoData
        )
    ) {
        return None;
    }
    let seen = usage.seen_at?;
    if seen > now || (!provider_working && now.saturating_sub(seen) > MAX_READING_AGE_SECS) {
        return None;
    }
    let kinds: &[WindowKind] = match usage.provider {
        Provider::Cursor => &[WindowKind::Auto, WindowKind::Named],
        _ => &[WindowKind::FiveHour],
    };
    usage
        .windows
        .iter()
        .filter(|w| kinds.contains(&w.kind))
        .filter_map(|w| {
            if w.reset
                || w.resets_at.is_some_and(|at| at <= now)
                || (w.kind == WindowKind::FiveHour
                    && w.resets_at
                        .is_some_and(|at| seen < at.saturating_sub(5 * 3600)))
            {
                return None;
            }
            w.left_pct.map(|p| p.min(100))
        })
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::UsageWindow;

    const P: Provider = Provider::Codex;

    fn id() -> TerminalId {
        TerminalId::alloc()
    }

    fn usage(
        provider: Provider,
        windows: &[(WindowKind, Option<u8>, Option<u64>)],
    ) -> ProviderUsage {
        ProviderUsage {
            extra_spend: None,
            openrouter: None,
            provider,
            windows: windows
                .iter()
                .map(|(kind, left, resets)| UsageWindow {
                    kind: *kind,
                    left_pct: *left,
                    resets_at: *resets,
                    note: None,
                    reset: false,
                })
                .collect(),
            seen_at: Some(5000),
            source: "test".into(),
            problem: None,
        }
    }

    #[test]
    fn defaults_match_the_brief() {
        let c = GuardConfig::default();
        assert_eq!(
            (
                c.wind_down_pct,
                c.fast_drop_points,
                c.fast_window_minutes,
                c.stop_pct
            ),
            (15, 25, 10, 3)
        );
        assert!(c.claude && c.codex && !c.cursor);
        let parsed: GuardConfig = toml::from_str("stop_pct = 5\ncursor = true\n").unwrap();
        assert_eq!(parsed.stop_pct, 5);
        assert_eq!(parsed.wind_down_pct, 15);
        assert!(parsed.cursor);
    }

    #[test]
    fn the_config_section_parses_and_unset_keys_keep_their_defaults() {
        let config: crate::config::Config =
            toml::from_str("[allowance_guard]\nwind_down_pct = 20\ncodex = false\n").unwrap();
        assert_eq!(config.allowance_guard.wind_down_pct, 20);
        assert!(!config.allowance_guard.codex);
        assert_eq!(config.allowance_guard.stop_pct, 3);
        let none: crate::config::Config = toml::from_str("").unwrap();
        assert_eq!(none.allowance_guard, GuardConfig::default());
    }

    #[test]
    fn above_the_line_does_nothing() {
        let mut g = Guard::default();
        assert!(g.observe(P, 80, 0).is_empty());
        assert!(g.observe(P, 16, 2000).is_empty());
        assert_eq!(g.last_pct(P), Some(16));
        assert!(!g.active(P));
    }

    #[test]
    fn at_the_wind_down_line_the_note_fires_once() {
        let mut g = Guard::default();
        g.observe(P, 30, 0);
        assert_eq!(
            g.observe(P, 15, 300),
            vec![Event::WindDown {
                provider: P,
                pct: 15,
                fast: false
            }]
        );
        assert!(g.active(P));
        assert!(g.observe(P, 14, 600).is_empty());
        assert!(g.observe(P, 10, 900).is_empty());
    }

    #[test]
    fn a_fast_drop_fires_even_when_far_from_the_line() {
        let mut g = Guard::default();
        g.observe(P, 90, 0);
        assert_eq!(
            g.observe(P, 60, 420),
            vec![Event::WindDown {
                provider: P,
                pct: 60,
                fast: true
            }],
        );
        assert!(g.active(P));
        // The same fall is not announced again by the next readings.
        assert!(g.observe(P, 58, 480).is_empty());
        assert!(g.observe(P, 55, 540).is_empty());
    }

    #[test]
    fn a_slow_fall_over_the_window_is_not_fast() {
        let mut g = Guard::default();
        g.observe(P, 60, 0);
        g.observe(P, 50, 400);
        // 60 left the ten minute window, 50 to 35 is 15 points
        assert!(g.observe(P, 35, 1100).is_empty());
    }

    #[test]
    fn stop_fires_once_at_the_stop_line() {
        let mut g = Guard::default();
        g.observe(P, 20, 0);
        let first = g.observe(P, 12, 300);
        assert_eq!(first.len(), 1);
        assert_eq!(
            g.observe(P, 3, 600),
            vec![Event::Stop {
                provider: P,
                pct: 3
            }]
        );
        assert!(g.observe(P, 2, 900).is_empty());
    }

    #[test]
    fn a_jump_straight_to_the_stop_line_does_both_in_order() {
        let mut g = Guard::default();
        let events = g.observe(P, 2, 0);
        assert_eq!(
            events,
            vec![
                Event::WindDown {
                    provider: P,
                    pct: 2,
                    fast: false
                },
                Event::Stop {
                    provider: P,
                    pct: 2
                }
            ]
        );
    }

    #[test]
    fn it_rearms_only_above_fifty_and_then_fires_again() {
        let mut g = Guard::default();
        g.observe(P, 10, 0);
        assert!(g.active(P));
        assert!(g.observe(P, 40, 100).is_empty());
        assert!(g.active(P), "40 is not enough to re-arm");
        assert!(g.observe(P, 51, 200).is_empty());
        assert!(!g.active(P));
        assert_eq!(g.observe(P, 12, 300).len(), 1);
    }

    #[test]
    fn a_fast_drop_to_sixty_rearms_above_fifty_not_above_sixty() {
        let mut g = Guard::default();
        let first = g.observe(P, 90, 0);
        assert!(first.is_empty());
        assert_eq!(g.observe(P, 60, 100).len(), 1, "90 to 60 is a fast drop");
        assert!(g.active(P));
        assert!(g.observe(P, 50, 200).is_empty());
        assert!(g.active(P), "50 is not above 50");
        assert!(g.observe(P, 51, 300).is_empty());
        assert!(!g.active(P), "51 re-arms");
        assert_eq!(
            g.observe(P, 8, 400).len(),
            1,
            "a later fall to 8 fires again"
        );
        assert!(g.observe(P, 51, 500).is_empty());
        assert!(!g.active(P), "51 re-arms again");
    }

    #[test]
    fn a_provider_switched_off_is_never_touched() {
        let mut g = Guard::new(GuardConfig {
            codex: false,
            ..GuardConfig::default()
        });
        assert!(g.observe(P, 1, 0).is_empty());
        assert!(!g.active(P));
        let mut g = Guard::default();
        assert!(
            g.observe(Provider::Cursor, 1, 0).is_empty(),
            "cursor is off by default"
        );
        let mut g = Guard::new(GuardConfig {
            cursor: true,
            ..GuardConfig::default()
        });
        assert_eq!(g.observe(Provider::Cursor, 1, 0).len(), 2);
    }

    #[test]
    fn providers_are_separate() {
        let mut g = Guard::default();
        g.observe(Provider::Codex, 10, 0);
        assert!(g.active(Provider::Codex));
        assert!(!g.active(Provider::Claude));
        assert_eq!(g.observe(Provider::Claude, 14, 0).len(), 1);
    }

    #[test]
    fn panes_are_noted_and_interrupted_once_per_window() {
        let mut g = Guard::default();
        let a = id();
        assert!(!g.needs_note(P, &a), "inactive provider needs no note");
        g.observe(P, 10, 0);
        assert!(g.needs_note(P, &a));
        g.mark_noted(P, &a);
        assert!(!g.needs_note(P, &a));
        assert!(!g.needs_interrupt(P, &a), "not stopped yet");
        g.observe(P, 3, 60);
        assert!(g.needs_interrupt(P, &a));
        g.mark_interrupted(P, &a);
        assert!(!g.needs_interrupt(P, &a));
        g.observe(P, 90, 120);
        g.observe(P, 10, 180);
        assert!(g.needs_note(P, &a), "a new window starts clean");
    }

    #[test]
    fn readings_use_the_five_hour_window_and_distrust_bad_ones() {
        let now = 5000;
        let u = usage(
            Provider::Codex,
            &[
                (WindowKind::FiveHour, Some(12), Some(now + 100)),
                (WindowKind::Weekly, Some(90), None),
            ],
        );
        assert_eq!(reading(&u, now, false), Some(12));
        assert_eq!(reading(&u, now, true), Some(12), "working provider");
        let over = usage(
            Provider::Codex,
            &[(WindowKind::FiveHour, Some(2), Some(now - 1))],
        );
        assert_eq!(
            reading(&over, now, false),
            None,
            "past its reset needs a fresh reading"
        );
        let mut bad = u.clone();
        bad.problem = Some(UsageProblem::LoginExpired);
        assert_eq!(reading(&bad, now, false), None);
        let none = usage(Provider::Codex, &[(WindowKind::FiveHour, None, None)]);
        assert_eq!(reading(&none, now, false), None);
        let cursor = usage(
            Provider::Cursor,
            &[
                (WindowKind::Auto, Some(40), None),
                (WindowKind::Named, Some(7), None),
            ],
        );
        assert_eq!(reading(&cursor, now, false), Some(7));
    }

    #[test]
    fn stale_or_previous_window_percentages_are_unknown_even_without_ui_staleness() {
        let now = 50_000;
        let mut u = usage(P, &[(WindowKind::FiveHour, Some(3), Some(now + 3600))]);
        u.seen_at = Some(now - 24_791);
        assert_eq!(reading(&u, now, false), None);
        assert_eq!(
            reading(&u, now, true),
            None,
            "previous window even while working"
        );
        u.seen_at = Some(now - MAX_READING_AGE_SECS - 1);
        assert_eq!(reading(&u, now, false), None);
        assert_eq!(
            reading(&u, now, true),
            Some(3),
            "working provider keeps current-window reading"
        );
        u.seen_at = None;
        assert_eq!(reading(&u, now, false), None);
        u.seen_at = Some(now - 60);
        u.windows[0].resets_at = Some(now + 5 * 3600 - 30);
        assert_eq!(
            reading(&u, now, false),
            None,
            "predates this five-hour window"
        );
        u.windows[0].resets_at = Some(now + 3600);
        assert_eq!(
            reading(&u, now, false),
            Some(3),
            "fresh low readings still stop"
        );
        u.windows[0].reset = true;
        assert_eq!(reading(&u, now, false), None);
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    use crate::usage::UsageWindow;
    fn usage(short: u8, long: u8, now: u64) -> ProviderUsage {
        ProviderUsage {
            extra_spend: None,
            openrouter: None,
            provider: Provider::Codex,
            seen_at: Some(now),
            source: "fixture".into(),
            problem: None,
            windows: vec![
                UsageWindow {
                    kind: WindowKind::FiveHour,
                    left_pct: Some(short),
                    resets_at: Some(now + 3600),
                    note: None,
                    reset: false,
                },
                UsageWindow {
                    kind: WindowKind::Weekly,
                    left_pct: Some(long),
                    resets_at: Some(now + 86400),
                    note: None,
                    reset: false,
                },
            ],
        }
    }
    #[test]
    fn provider_defaults_legacy_values_and_restart_roundtrip() {
        let c = GuardConfig::default();
        assert_eq!(c.effective(Provider::Codex).short.wind_down_pct, 5);
        assert_eq!(c.effective(Provider::Claude).short.wind_down_pct, 15);
        let legacy: GuardConfig = toml::from_str("wind_down_pct=15").unwrap();
        assert_eq!(legacy.effective(Provider::Codex).short.wind_down_pct, 15);
        assert!(!c.effective(Provider::Cursor).enabled);
        let mut config: crate::config::Config =
            toml::from_str("[allowance_guard]\nwind_down_pct=20\ncodex=false\n").unwrap();
        assert_eq!(
            config
                .allowance_guard
                .effective(Provider::Codex)
                .short
                .wind_down_pct,
            20
        );
        let mut p = c.effective(Provider::Codex);
        p.short.wind_down_pct = 5;
        config
            .allowance_guard
            .providers
            .insert(Provider::Codex, p.clone());
        let encoded = toml::to_string(&p).unwrap();
        let restarted: ProviderLimits = toml::from_str(&encoded).unwrap();
        assert_eq!(restarted, p);
        let mut g = Guard::new(c);
        assert!(g.observe_usage(&usage(15, 90, 100), 100, true).is_empty());
        assert!(matches!(
            g.observe_usage(&usage(5, 90, 200), 200, true).first(),
            Some(Event::WindDown { pct: 5, .. })
        ));
    }
    #[test]
    fn weekly_team_floor_keeps_overseer_reserve_and_stop_below_winddown() {
        let mut g = Guard::default();
        let mut policy = g.config.effective(Provider::Codex);
        policy.long.wind_down_pct = 17;
        policy.long.overseer_pct = 8;
        g.config.providers.insert(Provider::Codex, policy.clone());
        let pane = TerminalId::alloc();
        let u = usage(90, 17, 100);
        assert!(!g.observe_usage(&u, 100, true).is_empty());
        assert!(g
            .pane_action(Provider::Codex, false, &pane, &u, (100, true))
            .is_some());
        assert!(g
            .pane_action(Provider::Codex, true, &pane, &u, (100, true))
            .is_none());
        let u = usage(90, 8, 200);
        g.observe_usage(&u, 200, true);
        assert!(g
            .pane_action(Provider::Codex, true, &pane, &u, (200, true))
            .is_some());
        policy.short.stop_pct = policy.short.wind_down_pct;
        assert!(policy.validate().is_err());
        policy.short.stop_pct = policy.short.wind_down_pct + 1;
        assert!(policy.validate().is_err());
    }
    #[test]
    fn expired_low_is_estimated_full_rearms_and_real_low_replaces_it() {
        let mut g = Guard::default();
        let mut u = usage(3, 90, 100);
        g.observe_usage(&u, 100, true);
        assert!(g.active(Provider::Codex));
        u.windows[0].resets_at = Some(200);
        assert_eq!(window_reading(&u, &u.windows[0], 300, false), Some(100));
        g.observe_usage(&u, 300, false);
        assert!(!g.active(Provider::Codex));
        let fresh = usage(3, 90, 400);
        assert!(!g.observe_usage(&fresh, 400, true).is_empty());
        assert!(g.active(Provider::Codex));
    }
    #[test]
    fn independent_window_fast_history_does_not_invent_a_drop() {
        let mut g = Guard::default();
        g.observe_usage(&usage(99, 20, 100), 100, true);
        assert!(g.observe_usage(&usage(98, 19, 200), 200, true).is_empty());
        let mut u = usage(60, 90, 300);
        g.observe_usage(&u, 300, true);
        let pane = TerminalId::alloc();
        assert!(g
            .pane_action(Provider::Codex, false, &pane, &u, (300, true))
            .is_some());
        u.seen_at = Some(301);
        g.observe_usage(&u, 301, true);
        assert!(g.active(Provider::Codex));
    }
}

#[cfg(test)]
mod extra_spend_tests {
    use super::*;
    use crate::usage::CursorSpend;
    fn sample(bonus: u64, demand: u64, at: u64) -> ProviderUsage {
        ProviderUsage {
            provider: Provider::Cursor,
            openrouter: None,
            windows: vec![],
            seen_at: Some(at),
            source: "fixture".into(),
            problem: None,
            extra_spend: Some(CursorSpend {
                bonus_units: bonus,
                on_demand_units: demand,
                total_units: 10000,
                cycle_end: 10000,
            }),
        }
    }
    #[test]
    fn allowance_extra_spend_exact_thresholds_roles_reset_and_stale() {
        let mut g = Guard::default();
        g.config.cursor = true;
        let mut policy = g.config.effective(Provider::Cursor);
        policy.extra_spend.enabled = true;
        assert!(policy.validate().is_ok());
        g.config.providers.insert(Provider::Cursor, policy.clone());
        let pane = TerminalId::alloc();
        assert!(g.observe_usage(&sample(8400, 0, 101), 101, true).is_empty());
        let wind = sample(8500, 0, 102);
        assert!(matches!(
            g.observe_usage(&wind, 102, true).first(),
            Some(Event::WindDown { .. })
        ));
        assert!(
            g.pane_action(Provider::Cursor, false, &pane, &wind, (102, true))
                .unwrap()
                .3
        );
        assert!(g
            .pane_action(Provider::Cursor, true, &pane, &wind, (102, true))
            .is_none());
        let reserve = sample(8500, 700, 103);
        g.observe_usage(&reserve, 103, true);
        assert!(
            g.pane_action(Provider::Cursor, true, &pane, &reserve, (103, true))
                .unwrap()
                .3
        );
        let stop = sample(8500, 1200, 104);
        assert!(matches!(
            g.observe_usage(&stop, 104, true).last(),
            Some(Event::Stop { .. })
        ));
        for role in [false, true] {
            assert!(
                g.pane_action(Provider::Cursor, role, &pane, &stop, (104, true))
                    .unwrap()
                    .4
            );
            g.mark_policy_action(Provider::Cursor, role, &pane, true);
            assert!(
                !g.pane_action(Provider::Cursor, role, &pane, &stop, (104, true))
                    .unwrap()
                    .4
            );
        }
        assert!(g
            .pane_action(Provider::Cursor, false, &pane, &stop, (1005, true))
            .is_none());
        g.reset_provider(Provider::Cursor);
        let mut rollover = sample(0, 0, 105);
        rollover.extra_spend.as_mut().unwrap().cycle_end += 10000;
        assert!(g.observe_usage(&rollover, 105, true).is_empty());
        assert!(!g.active(Provider::Cursor));
        let encoded = toml::to_string(&policy).unwrap();
        assert_eq!(toml::from_str::<ProviderLimits>(&encoded).unwrap(), policy);
        let mut missing = stop.clone();
        missing.extra_spend = None;
        assert!(policy.extra_spend.window(&missing).is_none());
        let mut invalid = policy.clone();
        invalid.extra_spend.limits.stop_pct = 16;
        assert!(invalid.validate().is_err());
        assert_eq!(
            policy.extra_spend.window(&rollover).unwrap().left_pct,
            Some(100)
        );
    }
    #[test]
    fn allowance_extra_spend_never_masks_a_pool_stop() {
        let mut g = Guard::default();
        g.config.cursor = true;
        let mut p = g.config.effective(Provider::Cursor);
        p.extra_spend.enabled = true;
        g.config.providers.insert(Provider::Cursor, p);
        let mut u = sample(500, 0, 101);
        u.windows.push(crate::usage::UsageWindow {
            kind: WindowKind::Named,
            left_pct: Some(0),
            resets_at: Some(10000),
            note: None,
            reset: false,
        });
        assert!(g
            .observe_usage(&u, 101, true)
            .iter()
            .any(|e| matches!(e, Event::Stop { .. })));
    }
}

#[cfg(test)]
mod terminal_exception_tests {
    use super::*;

    #[test]
    fn exception_requires_matching_provider_time_and_nonzero_reserve() {
        let mut e = TerminalException {
            provider: Provider::Claude,
            starts_at: 100,
            expires_at: 200,
            minimum_remaining_pct: 2,
        };
        assert!(e.permits(Provider::Claude, 150, 3));
        assert!(!e.permits(Provider::Codex, 150, 3));
        assert!(!e.permits(Provider::Claude, 99, 3));
        assert!(!e.permits(Provider::Claude, 200, 3));
        assert!(!e.permits(Provider::Claude, 150, 2));
        e.expires_at = 3701;
        assert!(!e.permits(Provider::Claude, 150, 3));
        e.expires_at = 200;
        e.minimum_remaining_pct = 0;
        assert!(!e.permits(Provider::Claude, 150, 3));
    }

    #[test]
    fn config_retains_terminal_binding_without_disabling_provider() {
        let c: GuardConfig = toml::from_str(
            r#"
[terminal_exceptions.test_terminal]
provider = "claude"
starts_at = 100
expires_at = 200
minimum_remaining_pct = 2
"#,
        )
        .unwrap();
        assert!(c.watches(Provider::Claude));
        assert_eq!(c.terminal_exceptions.len(), 1);
        assert!(!c.terminal_exceptions.contains_key("other_terminal"));
    }
}

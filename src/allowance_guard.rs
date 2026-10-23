//! The allowance guard: when a provider's five hour allowance runs low, or
//! falls fast, every working agent on that provider is told to wind down, and
//! at the stop line is interrupted. This file holds the settings and the
//! decisions. It reads no clock and touches no pane, so it is tested with fake
//! readings. `app::allowance_guard` carries the decisions out.

use std::collections::{HashMap, HashSet, VecDeque};

use serde::Deserialize;

use crate::terminal::TerminalId;
use crate::usage::{Provider, ProviderUsage, UsageProblem, WindowKind};

/// A provider re-arms once its allowance is back above this.
const REARM_ABOVE_PCT: u8 = 50;

/// Cached percentages cannot authorize a note or interrupt indefinitely.
pub(crate) const MAX_READING_AGE_SECS: u64 = 15 * 60;

/// `[allowance_guard]`. Percentages are what is left of the five hour window.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
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
        }
    }
}

impl GuardConfig {
    pub fn watches(&self, provider: Provider) -> bool {
        match provider {
            Provider::Claude => self.claude,
            Provider::Codex => self.codex,
            Provider::Cursor => self.cursor,
        }
    }
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
    /// Panes already given the note, and panes already interrupted, this window.
    noted: HashSet<TerminalId>,
    interrupted: HashSet<TerminalId>,
}

#[derive(Debug, Default)]
pub struct Guard {
    pub config: GuardConfig,
    history: HashMap<Provider, VecDeque<(u64, u8)>>,
    last: HashMap<Provider, u8>,
    phases: HashMap<Provider, Phase>,
}

impl Guard {
    pub fn new(config: GuardConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    /// True while the guard is active on the provider, for the usage bar mark.
    pub fn active(&self, provider: Provider) -> bool {
        self.phases.contains_key(&provider)
    }

    pub fn any_active(&self) -> bool {
        !self.phases.is_empty()
    }

    /// True once per pane and window: the pane has not had the note yet.
    pub fn needs_note(&self, provider: Provider, pane: &TerminalId) -> bool {
        self.phases
            .get(&provider)
            .is_some_and(|p| !p.noted.contains(pane))
    }

    pub fn mark_noted(&mut self, provider: Provider, pane: &TerminalId) {
        if let Some(phase) = self.phases.get_mut(&provider) {
            phase.noted.insert(pane.clone());
        }
    }

    pub fn needs_interrupt(&self, provider: Provider, pane: &TerminalId) -> bool {
        self.phases
            .get(&provider)
            .is_some_and(|p| p.stopped && !p.interrupted.contains(pane))
    }

    pub fn mark_interrupted(&mut self, provider: Provider, pane: &TerminalId) {
        if let Some(phase) = self.phases.get_mut(&provider) {
            phase.interrupted.insert(pane.clone());
        }
    }

    /// Forgets panes that have gone, so the sets do not grow.
    pub fn retain_panes(&mut self, live: impl Fn(&TerminalId) -> bool) {
        for phase in self.phases.values_mut() {
            phase.noted.retain(|id| live(id));
            phase.interrupted.retain(|id| live(id));
        }
    }

    /// The percent the provider was last seen at.
    pub fn last_pct(&self, provider: Provider) -> Option<u8> {
        self.last.get(&provider).copied()
    }

    /// True when the guard was set off by a fast fall rather than the line.
    pub fn fast(&self, provider: Provider) -> bool {
        self.phases.get(&provider).is_some_and(|p| p.fast)
    }

    /// A provider whose reading cannot be used (no login, stale, switched off)
    /// takes no new action. An active phase stays until the allowance is seen
    /// back above 50%, so a missing reading never re-arms it by accident.
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

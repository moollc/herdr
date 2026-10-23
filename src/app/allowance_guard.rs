//! Carries out what `crate::allowance_guard` decides: the wind down note to
//! every working agent on a low provider, the interrupt at the stop line, and
//! one line to the overseer. Nothing here kills a process.
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use crate::allowance_guard::{self, Event};
use crate::app::App;
use crate::detect::{Agent, AgentState};
use crate::terminal::TerminalId;
use crate::ui::usage_bar::{provider_for_agent, provider_name};
use crate::usage::{Provider, UsageSnapshot};

/// How often panes are looked at while the guard is active on a provider.
const TICK: Duration = Duration::from_secs(2);
/// Output counts kept per pane, one per usage reading.
const SAMPLES: usize = 3;
/// Overseer lines waiting for it to be free; older ones are dropped past this.
const MAX_LINES: usize = 8;

pub(crate) struct GuardRuntime {
    pub next_tick: Instant,
    output: HashMap<TerminalId, VecDeque<u64>>,
    lines: VecDeque<String>,
    readings: Option<(UsageSnapshot, u64, Instant)>,
    refresh_requested: HashSet<Provider>,
}

impl Default for GuardRuntime {
    fn default() -> Self {
        Self {
            next_tick: Instant::now(),
            output: HashMap::new(),
            lines: VecDeque::new(),
            readings: None,
            refresh_requested: HashSet::new(),
        }
    }
}

fn wind_down_note(provider: Provider, pct: u8, fast: bool) -> String {
    let fall = if fast {
        format!("has fallen fast and is at {pct}%")
    } else {
        format!("is at {pct}%")
    };
    format!(
        "Herdr allowance guard: the {} five hour allowance {fall}. Finish this step, save your work, \
         write a hand-off or a short note of where you are, then stop. Start nothing new until it resets.",
        provider_name(provider)
    )
}

impl App {
    /// Reads the latest snapshot, feeds each provider's reading to the guard
    /// and carries out what it decides. Called when a usage reading lands.
    pub(crate) fn evaluate_allowance_guard(&mut self) {
        let Some(snapshot) = self.state.usage_snapshot.clone() else {
            return;
        };
        self.allowance_guard_readings(&snapshot, crate::usage::now());
    }

    pub(crate) fn allowance_guard_readings(&mut self, snapshot: &UsageSnapshot, now: u64) {
        self.sample_guard_output();
        let received = Instant::now();
        self.guard_run.readings = Some((snapshot.clone(), now, received));
        let trusted = self.trusted_guard_providers(snapshot, now);
        let mut events = Vec::new();
        for usage in &snapshot.providers {
            let working = !self.guard_panes(usage.provider).is_empty();
            if let Some(pct) = allowance_guard::reading(usage, now, working) {
                let resumed = self.guard_run.refresh_requested.contains(&usage.provider);
                let mut next = self.state.allowance_guard.observe(usage.provider, pct, now);
                if resumed && next.is_empty() && self.state.allowance_guard.active(usage.provider) {
                    next.push(Event::WindDown {
                        provider: usage.provider,
                        pct,
                        fast: self.state.allowance_guard.fast(usage.provider),
                    });
                }
                events.extend(next);
            } else if !self.state.allowance_guard.config.watches(usage.provider) {
                self.state.allowance_guard.observe(usage.provider, 100, now);
            }
        }
        self.pause_unknown_guard(snapshot, &trusted, received);
        if !events.is_empty() {
            self.announce_guard_events(&events);
        }
        self.guard_run.next_tick = Instant::now();
        self.render_dirty
            .store(true, std::sync::atomic::Ordering::Release);
    }

    fn trusted_guard_providers(&self, snapshot: &UsageSnapshot, now: u64) -> HashSet<Provider> {
        snapshot
            .providers
            .iter()
            .filter_map(|usage| {
                let working = !self.guard_panes(usage.provider).is_empty();
                allowance_guard::reading(usage, now, working).map(|_| usage.provider)
            })
            .collect()
    }

    /// Unknown suspends actions without inventing recovery or losing pane deduplication.
    /// Queue one read-only refresh per unknown episode; periodic refreshes still run.
    fn pause_unknown_guard(
        &mut self,
        snapshot: &UsageSnapshot,
        trusted: &HashSet<Provider>,
        now: Instant,
    ) {
        for provider in [Provider::Claude, Provider::Codex, Provider::Cursor] {
            if trusted.contains(&provider) {
                self.guard_run.refresh_requested.remove(&provider);
                continue;
            }
            let prefix = format!(
                "Herdr allowance guard: {} five hour",
                provider_name(provider)
            );
            self.guard_run
                .lines
                .retain(|line| !line.starts_with(&prefix));
            let relevant = snapshot.providers.iter().any(|u| u.provider == provider)
                || self.state.allowance_guard.active(provider)
                || !self.guard_panes(provider).is_empty();
            if relevant
                && self.state.allowance_guard.config.watches(provider)
                && self.guard_run.refresh_requested.insert(provider)
            {
                self.mark_usage_refresh_due(now);
            }
        }
    }

    fn sample_guard_output(&mut self) {
        self.guard_run
            .output
            .retain(|id, _| self.state.terminals.contains_key(id));
        for id in self.state.terminals.keys() {
            let Some(runtime) = self.terminal_runtimes.get(id) else {
                continue;
            };
            let samples = self.guard_run.output.entry(id.clone()).or_default();
            samples.push_back(runtime.output_bytes());
            while samples.len() > SAMPLES {
                samples.pop_front();
            }
        }
    }

    /// Working panes whose agent runs on `provider`, in any space.
    fn guard_panes(&self, provider: Provider) -> Vec<TerminalId> {
        let mut ids: Vec<_> = self
            .state
            .terminals
            .iter()
            .filter(|(_, t)| t.state == AgentState::Working)
            .filter(|(_, t)| {
                t.effective_known_agent().and_then(provider_for_agent) == Some(provider)
            })
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort_by_key(|id| id.to_string());
        ids
    }

    fn guard_name(&self, id: &TerminalId) -> String {
        crate::pane_names::effective_name(&self.state.terminals, id)
            .unwrap_or_else(|| id.to_string())
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The pane that wrote the most output across the last readings. Output
    /// volume is the closest sign of burn Herdr can see from outside the agent.
    fn guard_fastest(&self, panes: &[TerminalId]) -> Option<TerminalId> {
        panes
            .iter()
            .max_by_key(|id| {
                self.guard_run.output.get(*id).map_or(0, |s| {
                    let last = s.back().copied().unwrap_or(0);
                    if s.len() > 1 {
                        last.saturating_sub(s.front().copied().unwrap_or(0))
                    } else {
                        last
                    }
                })
            })
            .cloned()
    }

    fn announce_guard_events(&mut self, events: &[Event]) {
        for event in events {
            let (provider, pct) = match event {
                Event::WindDown { provider, pct, .. } | Event::Stop { provider, pct } => {
                    (*provider, *pct)
                }
            };
            let panes = self.guard_panes(provider);
            let names = panes
                .iter()
                .map(|id| self.guard_name(id))
                .collect::<Vec<_>>()
                .join(", ");
            let who = if names.is_empty() {
                "nothing working on it".to_string()
            } else {
                names
            };
            let line = match event {
                Event::WindDown { fast, .. } => {
                    let burning = self
                        .guard_fastest(&panes)
                        .map(|id| format!("; burning fastest: {}", self.guard_name(&id)))
                        .unwrap_or_default();
                    format!(
                        "Herdr allowance guard: {} five hour allowance {} at {pct}%. Told to wind down: {who}{burning}.",
                        provider_name(provider),
                        if *fast { "fell fast, now" } else { "is" },
                    )
                }
                Event::Stop { .. } => format!(
                    "Herdr allowance guard: {} five hour allowance at {pct}%. Interrupting with each agent's own interrupt key, nothing is killed: {who}.",
                    provider_name(provider),
                ),
            };
            tracing::info!(%line, "allowance guard");
            self.guard_run.lines.push_back(line);
        }
        while self.guard_run.lines.len() > MAX_LINES {
            self.guard_run.lines.pop_front();
        }
    }

    pub(crate) fn allowance_guard_deadline(&self) -> Option<Instant> {
        (self.state.allowance_guard.any_active() || !self.guard_run.lines.is_empty())
            .then_some(self.guard_run.next_tick)
    }

    /// Once a tick while the guard is active: note and interrupt panes that
    /// have not had it yet, and pass queued lines to the overseer.
    pub(crate) fn advance_allowance_guard(&mut self, now: Instant) -> bool {
        if self.allowance_guard_deadline().is_none() || now < self.guard_run.next_tick {
            return false;
        }
        self.guard_run.next_tick = now + TICK;
        let live: Vec<_> = self.state.terminals.keys().cloned().collect();
        self.state
            .allowance_guard
            .retain_panes(|id| live.contains(id));
        let trusted = if let Some((snapshot, epoch, received)) = self.guard_run.readings.clone() {
            let epoch_now = epoch.saturating_add(now.saturating_duration_since(received).as_secs());
            let trusted = self.trusted_guard_providers(&snapshot, epoch_now);
            self.pause_unknown_guard(&snapshot, &trusted, now);
            trusted
        } else {
            HashSet::new()
        };
        let mut changed = false;
        for provider in [Provider::Claude, Provider::Codex, Provider::Cursor] {
            if !trusted.contains(&provider) || !self.state.allowance_guard.active(provider) {
                continue;
            }
            for id in self.guard_panes(provider) {
                changed |= self.guard_pane(provider, &id, now);
            }
        }
        self.flush_guard_lines(now);
        changed
    }

    fn guard_pane(&mut self, provider: Provider, id: &TerminalId, now: Instant) -> bool {
        let Some(runtime) = self.terminal_runtimes.get(id) else {
            return false;
        };
        let needs_note = self.state.allowance_guard.needs_note(provider, id);
        let needs_interrupt = self.state.allowance_guard.needs_interrupt(provider, id);
        if needs_note {
            // The safe path: only into an empty prompt nobody is typing in.
            let quiet = runtime.input_is_quiet(now)
                && crate::overseer::empty_prompt_ansi(
                    &runtime.detection_text(),
                    &runtime.recent_ansi(80),
                );
            if quiet {
                let pct = self.state.allowance_guard.last_pct(provider).unwrap_or(0);
                let fast = self.state.allowance_guard.fast(provider);
                let text = wind_down_note(provider, pct, fast);
                if super::api_helpers::send_prompt(runtime, &text).is_ok() {
                    self.state.allowance_guard.mark_noted(provider, id);
                    return true;
                }
            }
        }
        if needs_interrupt {
            let agent = self
                .state
                .terminals
                .get(id)
                .and_then(|t| t.effective_known_agent());
            // Claude Code and Codex stop on Escape; Cursor's agent stops on Ctrl+C.
            let key = if agent == Some(Agent::Cursor) {
                "C-c"
            } else {
                "Escape"
            };
            let sent = super::api_helpers::encode_api_keys(runtime, &[key.into()])
                .ok()
                .map(|keys| keys.into_iter().flatten().collect::<Vec<u8>>())
                .is_some_and(|keys| {
                    super::api_helpers::deliver_text_then_keys(runtime, &[], &keys).is_ok()
                });
            if sent {
                self.state.allowance_guard.mark_interrupted(provider, id);
                return true;
            }
        }
        false
    }

    fn flush_guard_lines(&mut self, now: Instant) {
        let Some(line) = self.guard_run.lines.front().cloned() else {
            return;
        };
        // The allowance is the whole session's, so the chief hears it first;
        // without a chief, any overseer does.
        let holder = self
            .state
            .terminals
            .iter()
            .filter(|(_, t)| t.overseer.active)
            .min_by_key(|(_, t)| t.overseer.kind() != Some(crate::overseer::Kind::Chief))
            .map(|(id, _)| id.clone());
        match holder {
            Some(id) => {
                if self.overseer_ready(&id, now) && self.send_overseer(&id, &line) {
                    self.guard_run.lines.pop_front();
                }
            }
            None => {
                // No overseer in this session: the pane named by alert_pane, if any.
                self.ping_alert_pane(&line);
                self.guard_run.lines.pop_front();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppState;
    use crate::terminal::{TerminalRuntime, TerminalState};
    use crate::usage::{ProviderUsage, UsageWindow, WindowKind};
    use tokio::sync::mpsc::Receiver;

    const NOW: u64 = 2_000_000;

    struct Fake {
        id: TerminalId,
        rx: Receiver<bytes::Bytes>,
    }

    fn new_app() -> App {
        let (_, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state = AppState::test_new();
        app.state.mode = crate::app::Mode::Terminal;
        app
    }

    /// A fake agent pane in its own space: the agent, its state, a screen with an empty prompt.
    fn add(app: &mut App, space: &str, name: &str, agent: Agent, state: AgentState) -> Fake {
        let ws = crate::workspace::Workspace::test_new(space);
        let pane = ws.tabs[0].root_pane;
        let id = ws.terminal_id(pane).unwrap().clone();
        let mut t = TerminalState::new(id.clone(), std::env::temp_dir());
        t.detected_agent = Some(agent);
        t.state = state;
        t.agent_name = Some(name.into());
        let (rt, rx) = TerminalRuntime::test_with_channel(80, 24);
        rt.test_process_pty_bytes(b"working\r\n\xe2\x80\xba \r\n");
        app.terminal_runtimes.insert(id.clone(), rt);
        app.state.terminals.insert(id.clone(), t);
        app.state.workspaces.push(ws);
        app.state.active = Some(0);
        Fake { id, rx }
    }

    fn reading(provider: Provider, pct: u8, at: u64) -> UsageSnapshot {
        UsageSnapshot {
            version: 1,
            taken_at: at,
            providers: vec![ProviderUsage {
                provider,
                windows: vec![UsageWindow {
                    kind: if provider == Provider::Cursor {
                        WindowKind::Auto
                    } else {
                        WindowKind::FiveHour
                    },
                    left_pct: Some(pct),
                    resets_at: Some(at + 3 * 3600),
                    note: None,
                    reset: false,
                }],
                seen_at: Some(at),
                source: "fake".into(),
                problem: None,
            }],
        }
    }

    fn feed(app: &mut App, provider: Provider, pct: u8, at: u64) {
        app.allowance_guard_readings(&reading(provider, pct, at), at);
    }

    /// Runs the guard tick a few seconds apart, as the main loop would.
    fn ticks(app: &mut App, n: u64) {
        let start = Instant::now();
        for i in 0..n {
            app.advance_allowance_guard(start + Duration::from_secs(3 * (i + 1)));
        }
    }

    async fn typed(fake: &mut Fake) -> String {
        let mut out = String::new();
        while let Ok(Some(bytes)) =
            tokio::time::timeout(Duration::from_millis(900), fake.rx.recv()).await
        {
            out.push_str(&String::from_utf8_lossy(&bytes));
        }
        out
    }

    #[tokio::test]
    async fn wind_down_reaches_every_working_pane_on_the_provider_in_any_space_once() {
        let mut app = new_app();
        let mut a = add(
            &mut app,
            "Resolve",
            "Alpha",
            Agent::Codex,
            AgentState::Working,
        );
        let mut b = add(
            &mut app,
            "Build",
            "Bravo",
            Agent::Codex,
            AgentState::Working,
        );
        let mut idle = add(&mut app, "Build", "Idle", Agent::Codex, AgentState::Idle);
        let mut claude = add(
            &mut app,
            "Docs",
            "Delta",
            Agent::Claude,
            AgentState::Working,
        );
        feed(&mut app, Provider::Codex, 14, NOW);
        ticks(&mut app, 3);
        for fake in [&mut a, &mut b] {
            let got = typed(fake).await;
            assert!(got.contains("Herdr allowance guard"), "{got}");
            assert!(got.contains("GPT five hour allowance is at 14%"), "{got}");
            assert!(got.contains("write a hand-off"), "{got}");
            assert_eq!(got.matches("Herdr allowance guard").count(), 1, "once");
        }
        assert_eq!(typed(&mut idle).await, "", "an idle pane is not touched");
        assert_eq!(
            typed(&mut claude).await,
            "",
            "another provider is not touched"
        );
        assert!(app.state.allowance_guard.active(Provider::Codex));
        assert!(!app.state.allowance_guard.active(Provider::Claude));
        // a later reading in the same window does not repeat the note
        feed(&mut app, Provider::Codex, 12, NOW + 300);
        ticks(&mut app, 3);
        assert_eq!(typed(&mut a).await, "");
    }

    #[tokio::test]
    async fn a_fast_drop_notes_the_panes_even_with_plenty_left() {
        let mut app = new_app();
        let mut a = add(
            &mut app,
            "Resolve",
            "Alpha",
            Agent::Codex,
            AgentState::Working,
        );
        feed(&mut app, Provider::Codex, 100, NOW);
        ticks(&mut app, 2);
        assert_eq!(typed(&mut a).await, "");
        feed(&mut app, Provider::Codex, 40, NOW + 420);
        ticks(&mut app, 3);
        let got = typed(&mut a).await;
        assert!(got.contains("has fallen fast and is at 40%"), "{got}");
    }

    #[tokio::test]
    async fn the_stop_line_interrupts_with_escape_for_claude_and_codex_and_ctrl_c_for_cursor() {
        let mut app = new_app();
        let mut codex = add(&mut app, "A", "Cx", Agent::Codex, AgentState::Working);
        feed(&mut app, Provider::Codex, 3, NOW);
        ticks(&mut app, 4);
        let got = typed(&mut codex).await;
        assert!(got.contains('\u{1b}'), "Escape was sent: {got:?}");
        assert_eq!(got.matches('\u{1b}').count(), 1, "once");
        assert!(
            app.terminal_runtimes.get(&codex.id).is_some(),
            "nothing is killed"
        );

        let mut app = new_app();
        app.state.allowance_guard.config.cursor = true;
        let mut cursor = add(&mut app, "A", "Cu", Agent::Cursor, AgentState::Working);
        feed(&mut app, Provider::Cursor, 2, NOW);
        ticks(&mut app, 4);
        assert!(typed(&mut cursor).await.contains('\u{3}'));
    }

    #[tokio::test]
    async fn the_note_goes_first_and_waits_for_an_empty_quiet_prompt() {
        let mut app = new_app();
        let mut a = add(&mut app, "A", "Cx", Agent::Codex, AgentState::Working);
        app.terminal_runtimes
            .get(&a.id)
            .unwrap()
            .record_input_bytes(b"half a thought");
        feed(&mut app, Provider::Codex, 10, NOW);
        ticks(&mut app, 3);
        assert_eq!(
            typed(&mut a).await,
            "",
            "a draft in the prompt is never typed over"
        );
        // the draft is sent, and two quiet seconds pass
        app.terminal_runtimes
            .get(&a.id)
            .unwrap()
            .record_input_bytes(b"\r");
        let later = Instant::now() + Duration::from_secs(30);
        app.advance_allowance_guard(later);
        assert!(typed(&mut a).await.contains("Herdr allowance guard"));
    }

    #[tokio::test]
    async fn the_guard_rearms_after_the_allowance_recovers() {
        let mut app = new_app();
        let mut a = add(&mut app, "A", "Cx", Agent::Codex, AgentState::Working);
        feed(&mut app, Provider::Codex, 10, NOW);
        ticks(&mut app, 3);
        assert!(typed(&mut a).await.contains("guard"));
        feed(&mut app, Provider::Codex, 45, NOW + 600);
        assert!(
            app.state.allowance_guard.active(Provider::Codex),
            "45 is not recovered"
        );
        feed(&mut app, Provider::Codex, 80, NOW + 1200);
        assert!(!app.state.allowance_guard.active(Provider::Codex));
        feed(&mut app, Provider::Codex, 9, NOW + 1800);
        ticks(&mut app, 3);
        assert!(
            typed(&mut a).await.contains("guard"),
            "a new window is noted again"
        );
    }

    #[tokio::test]
    async fn cursor_is_left_alone_by_default_and_a_switched_off_provider_too() {
        let mut app = new_app();
        let mut cursor = add(&mut app, "A", "Cu", Agent::Cursor, AgentState::Working);
        feed(&mut app, Provider::Cursor, 1, NOW);
        ticks(&mut app, 3);
        assert_eq!(typed(&mut cursor).await, "");
        assert!(!app.state.allowance_guard.active(Provider::Cursor));
        app.state.allowance_guard.config.codex = false;
        let mut cx = add(&mut app, "A", "Cx", Agent::Codex, AgentState::Working);
        feed(&mut app, Provider::Codex, 1, NOW);
        ticks(&mut app, 3);
        assert_eq!(typed(&mut cx).await, "");
    }

    #[tokio::test]
    async fn an_unreadable_provider_does_nothing() {
        let mut app = new_app();
        let mut a = add(&mut app, "A", "Cx", Agent::Codex, AgentState::Working);
        let mut snap = reading(Provider::Codex, 1, NOW);
        snap.providers[0].problem = Some(crate::usage::UsageProblem::LoginExpired);
        app.allowance_guard_readings(&snap, NOW);
        ticks(&mut app, 3);
        assert_eq!(typed(&mut a).await, "");
        // A previous-window reading is unknown even while a pane works.
        let mut snap = reading(Provider::Codex, 1, NOW);
        snap.providers[0].seen_at = Some(NOW - 3 * 3600);
        app.allowance_guard_readings(&snap, NOW);
        assert!(!app.state.allowance_guard.active(Provider::Codex));
    }

    #[tokio::test]
    async fn the_guard_line_goes_to_the_chief_and_not_to_a_space_overseer() {
        let mut app = new_app();
        let mut space = add(&mut app, "A", "Fern", Agent::Claude, AgentState::Idle);
        let mut chief = add(&mut app, "B", "Moss", Agent::Claude, AgentState::Idle);
        let _worker = add(&mut app, "C", "Reed", Agent::Codex, AgentState::Working);
        for (fake, kind) in [
            (&space, crate::overseer::Kind::Space),
            (&chief, crate::overseer::Kind::Chief),
        ] {
            let t = app.state.terminals.get_mut(&fake.id).unwrap();
            t.overseer.assign(Some(kind));
            t.overseer.notices.clear();
        }
        feed(&mut app, Provider::Codex, 14, NOW);
        ticks(&mut app, 3);
        let got = typed(&mut chief).await;
        assert!(got.contains("GPT five hour allowance"), "{got}");
        assert_eq!(
            typed(&mut space).await,
            "",
            "a space overseer is not the chief"
        );
    }

    #[tokio::test]
    async fn the_overseer_gets_one_line_naming_provider_percent_and_the_busiest_pane() {
        let mut app = new_app();
        let mut boss = add(&mut app, "Team", "Chief", Agent::Claude, AgentState::Idle);
        app.state
            .terminals
            .get_mut(&boss.id)
            .unwrap()
            .overseer
            .active = true;
        let _quiet = add(&mut app, "A", "Bravo", Agent::Codex, AgentState::Working);
        let loud = add(&mut app, "B", "Alpha", Agent::Codex, AgentState::Working);
        // two readings, Alpha writes a lot between them
        feed(&mut app, Provider::Codex, 60, NOW);
        let big = vec![b'x'; 40_000];
        app.terminal_runtimes
            .get(&loud.id)
            .unwrap()
            .test_process_pty_bytes(&big);
        feed(&mut app, Provider::Codex, 14, NOW + 300);
        ticks(&mut app, 3);
        let got = typed(&mut boss).await;
        assert!(got.contains("GPT five hour allowance"), "{got}");
        assert!(got.contains("14%"), "{got}");
        assert!(got.contains("burning fastest: Alpha"), "{got}");
        assert!(got.contains("Bravo"), "{got}");
        assert_eq!(got.matches("Herdr allowance guard").count(), 1, "{got}");
        // and it says plainly when it interrupts
        feed(&mut app, Provider::Codex, 3, NOW + 600);
        ticks(&mut app, 3);
        let got = typed(&mut boss).await;
        assert!(got.contains("nothing is killed"), "{got}");
        assert!(got.contains("3%"), "{got}");
    }

    #[tokio::test]
    async fn the_deadline_exists_only_while_there_is_work() {
        let mut app = new_app();
        let _a = add(&mut app, "A", "Cx", Agent::Codex, AgentState::Working);
        assert!(app.allowance_guard_deadline().is_none());
        feed(&mut app, Provider::Codex, 10, NOW);
        assert!(app.allowance_guard_deadline().is_some());
        feed(&mut app, Provider::Codex, 90, NOW + 100);
        app.guard_run.lines.clear();
        assert!(app.allowance_guard_deadline().is_none());
    }

    #[tokio::test]
    async fn stale_reset_readings_never_stop_a_newly_working_pane() {
        let mut app = new_app();
        let mut pane = add(&mut app, "Test", "Maple", Agent::Codex, AgentState::Working);
        let mut snapshot = reading(Provider::Codex, 3, NOW);
        snapshot.providers[0].seen_at = Some(NOW - 24_791);
        snapshot.providers[0].windows[0].resets_at = Some(NOW - 5 * 3600);
        app.allowance_guard_readings(&snapshot, NOW);
        ticks(&mut app, 3);
        assert_eq!(typed(&mut pane).await, "");
        assert!(!app.state.allowance_guard.active(Provider::Codex));
        assert!(app.guard_run.refresh_requested.contains(&Provider::Codex));
        assert!(app
            .usage_refresh_deadline()
            .is_some_and(|at| at <= Instant::now()));
        feed(&mut app, Provider::Codex, 99, NOW + 60);
        ticks(&mut app, 2);
        assert_eq!(typed(&mut pane).await, "");
        assert!(!app.guard_run.refresh_requested.contains(&Provider::Codex));
    }

    #[tokio::test]
    async fn old_current_window_reading_is_unknown_only_while_provider_is_idle() {
        let mut app = new_app();
        let pane = add(&mut app, "Test", "Maple", Agent::Codex, AgentState::Idle);
        let _other = add(
            &mut app,
            "Other",
            "Birch",
            Agent::Claude,
            AgentState::Working,
        );
        let mut snapshot = reading(Provider::Codex, 3, NOW);
        snapshot.providers[0].seen_at = Some(NOW - 901);
        app.allowance_guard_readings(&snapshot, NOW);
        assert!(!app.state.allowance_guard.active(Provider::Codex));
        app.state.terminals.get_mut(&pane.id).unwrap().state = AgentState::Working;
        app.allowance_guard_readings(&snapshot, NOW);
        assert!(app.state.allowance_guard.active(Provider::Codex));
        assert!(!app.guard_run.refresh_requested.contains(&Provider::Codex));
        let tick = Instant::now() + Duration::from_secs(901);
        app.advance_allowance_guard(tick);
        assert!(!app.guard_run.refresh_requested.contains(&Provider::Codex));
        app.state.terminals.get_mut(&pane.id).unwrap().state = AgentState::Idle;
        app.advance_allowance_guard(tick + TICK);
        assert!(app.guard_run.refresh_requested.contains(&Provider::Codex));
    }

    #[tokio::test]
    async fn unknown_suspends_an_active_stop_and_fresh_low_resumes_it() {
        let mut app = new_app();
        let mut pane = add(&mut app, "Test", "Maple", Agent::Codex, AgentState::Working);
        feed(&mut app, Provider::Codex, 3, NOW);
        let mut snapshot = reading(Provider::Codex, 3, NOW);
        snapshot.providers[0].seen_at = None;
        app.allowance_guard_readings(&snapshot, NOW);
        ticks(&mut app, 3);
        assert_eq!(
            typed(&mut pane).await,
            "",
            "active phase cannot act on unknown"
        );
        assert!(
            app.state.allowance_guard.active(Provider::Codex),
            "unknown is not recovery"
        );
        feed(&mut app, Provider::Codex, 3, NOW + 60);
        ticks(&mut app, 3);
        let got = typed(&mut pane).await;
        assert!(got.contains("allowance guard"));
        assert!(got.contains('\u{1b}'), "fresh low reading still interrupts");
    }

    #[tokio::test]
    async fn a_reading_that_passes_reset_between_updates_cannot_interrupt() {
        let mut app = new_app();
        let mut pane = add(&mut app, "Test", "Maple", Agent::Codex, AgentState::Working);
        feed(&mut app, Provider::Codex, 3, NOW);
        app.advance_allowance_guard(Instant::now() + Duration::from_secs(3 * 3600 + 1));
        assert_eq!(typed(&mut pane).await, "");
        assert!(
            app.guard_run.lines.is_empty(),
            "expired queued alarms are discarded"
        );
        assert!(app.guard_run.refresh_requested.contains(&Provider::Codex));
    }
}

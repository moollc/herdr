//! User-owned role state and bounded, input-free situation reporting.
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::detect::AgentState;
use crate::terminal::TerminalId;

/// Whether one overseer covers the whole session or each space may have its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    #[default]
    Session,
    Workspace,
}
/// What a role covers: one space, or every space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Space,
    Chief,
}
impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Space => "space",
            Self::Chief => "chief",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OverseerConfig {
    pub scope: Scope,
    pub note_file: Option<String>,
    pub events: bool,
    pub event_interval_seconds: u64,
    pub event_settle_seconds: u64,
    pub claude_settings: Option<String>,
    pub codex_args: Option<Vec<String>>,
    pub cursor_args: Option<Vec<String>>,
}
impl Default for OverseerConfig {
    fn default() -> Self {
        Self {
            scope: Scope::Session,
            note_file: None,
            events: true,
            event_interval_seconds: 20,
            event_settle_seconds: 20,
            claude_settings: None,
            codex_args: None,
            cursor_args: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Role {
    pub active: bool,
    /// Absent in sessions saved before space overseers: those held the one
    /// overseer of the whole session, which is the chief.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<Kind>,
    pub notices: VecDeque<Notice>,
    pub profile_base: Option<Vec<String>>,
    pub restart_argv: Option<Vec<String>>,
    pub input_pending: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notice {
    pub active: bool,
    pub profile_checked: bool,
    pub detail: String,
    #[serde(default)]
    pub silent: bool,
}
impl Role {
    pub fn restored(mut self, live: bool) -> Self {
        if !live {
            self.input_pending = false;
            self.restart_argv = None;
            for notice in &mut self.notices {
                notice.profile_checked = false;
            }
            if self.active && self.profile_base.is_some() && self.notices.is_empty() {
                self.notices.push_back(Notice {
                    active: true,
                    profile_checked: false,
                    detail: String::new(),
                    silent: true,
                });
            }
        }
        self
    }

    pub fn pending_restart(&self) -> Option<crate::app::AgentRestart> {
        self.restart_argv
            .as_ref()
            .map(|argv| crate::app::AgentRestart {
                argv: argv.clone(),
                started: Instant::now(),
                shell_free_since: None,
            })
    }

    pub fn kind(&self) -> Option<Kind> {
        self.active.then(|| self.kind.unwrap_or(Kind::Chief))
    }

    /// Take, change or give up the role. A change between space and chief
    /// while held queues a fresh role note and leaves the permission profile
    /// alone, so the agent is not restarted for it.
    pub fn assign(&mut self, kind: Option<Kind>) {
        let before = self.kind();
        if before == kind {
            return;
        }
        match kind {
            None => {
                self.active = false;
                self.kind = None;
                self.notices.push_back(Notice {
                    active: false,
                    profile_checked: false,
                    detail: String::new(),
                    silent: false,
                });
            }
            Some(kind) => {
                let held = self.active;
                self.active = true;
                self.kind = Some(kind);
                let waiting = self.notices.back().is_some_and(|n| n.active && !n.silent);
                if !(held && waiting) {
                    self.notices.push_back(Notice {
                        active: true,
                        profile_checked: held,
                        detail: String::new(),
                        silent: false,
                    });
                }
            }
        }
    }
}

/// A candidate is retained only while the observed state and blocker agree.
pub struct PendingEvent {
    pub text: String,
    pub state: AgentState,
    pub reason: Option<String>,
    pub since: Instant,
    /// The consumed assignment still validates a pending turn outcome.
    pub marker: Option<PathBuf>,
}

/// Event keys that start with this are lines sent between overseers. They are
/// never revalidated against a pane and wait for no settle time.
pub const MAIL_PREFIX: &str = "msg:";
pub const MAIL_LIMIT: usize = 8;
pub const MAIL_LOG: usize = 10;
pub const MAIL_CHARS: usize = 240;

/// What one overseer has been told and has yet to be sent.
pub struct Holder {
    pub kind: Kind,
    pub written_at: Option<Instant>,
    /// The situation file last written, so it can say when the role ends.
    pub file: Option<PathBuf>,
    pub sent_at: Option<Instant>,
    pub events: BTreeMap<String, PendingEvent>,
    /// The latest lines from other overseers, for the situation file.
    pub mail: VecDeque<String>,
}
impl Holder {
    pub fn new(kind: Kind, now: Instant) -> Self {
        Self {
            kind,
            written_at: None,
            file: None,
            // The first batch waits a full interval after the role is taken.
            sent_at: Some(now),
            events: BTreeMap::new(),
            mail: VecDeque::new(),
        }
    }
    pub fn mail_waiting(&self) -> usize {
        self.events
            .keys()
            .filter(|key| key.starts_with(MAIL_PREFIX))
            .count()
    }
}

/// A missing hand-off can be an ordinary question. Wait for two minutes of
/// silence, then report once for this registration without consuming it.
pub const TASK_STOP_SETTLE_SECONDS: u64 = 120;

pub struct DirectStop {
    pub marker: PathBuf,
    seq: u64,
    since: Option<Instant>,
    alerted: bool,
}
impl DirectStop {
    pub fn new(marker: PathBuf, seq: u64) -> Self {
        Self {
            marker,
            seq,
            since: None,
            alerted: false,
        }
    }

    pub fn observe(&mut self, seq: u64, idle: bool, ended: bool, now: Instant) -> bool {
        // A start/end pair can arrive between ticks. The native sequence still
        // cancels the old question timer even if we never observe Working.
        if seq != self.seq || !idle {
            self.since = None;
            self.seq = seq;
        }
        if self.alerted || !idle {
            return false;
        }
        if ended {
            self.since = Some(now);
        }
        if self
            .since
            .is_some_and(|since| now.duration_since(since).as_secs() >= TASK_STOP_SETTLE_SECONDS)
        {
            self.alerted = true;
            self.since = None;
            return true;
        }
        false
    }
}

pub struct Service {
    pub config: OverseerConfig,
    pub tasks_path: Option<PathBuf>,
    /// Dispatcher stage ownership; read without mutating the queue.
    pub queue_path: PathBuf,
    /// The chief's file. A space overseer's file sits beside it.
    pub situation_path: PathBuf,
    pub next_tick: Instant,
    pub holders: HashMap<TerminalId, Holder>,
    pub mail_seq: u64,
    pub previous: HashMap<TerminalId, (AgentState, Option<String>)>,
    pub turn_ends: HashMap<TerminalId, u64>,
    pub direct_stops: HashMap<TerminalId, DirectStop>,
    pub profile_probes: HashMap<TerminalId, Vec<String>>,
}
impl Service {
    pub fn new(config: &crate::config::Config) -> Self {
        Self {
            config: config.overseer.clone(),
            tasks_path: crate::tasks::configured_path(config.tasks_file.as_deref()),
            queue_path: std::env::var_os("TEAM_QUEUE")
                .map(PathBuf::from)
                .unwrap_or_else(|| expand_path("~/model-bench-runs/team/queue.tsv")),
            situation_path: {
                let root = crate::config::state_dir();
                crate::session::active_name()
                    .map(|name| root.join("sessions").join(name))
                    .unwrap_or(root)
                    .join("overseer/situation.md")
            },
            next_tick: Instant::now(),
            holders: HashMap::new(),
            mail_seq: 0,
            previous: HashMap::new(),
            turn_ends: HashMap::new(),
            direct_stops: HashMap::new(),
            profile_probes: HashMap::new(),
        }
    }
    #[cfg(test)]
    pub fn events_empty(&self) -> bool {
        self.holders.values().all(|holder| holder.events.is_empty())
    }
}

/// The part of the role note that depends on what the role covers.
pub fn role_suffix(kind: Kind, space: Option<&str>) -> String {
    match kind {
        Kind::Space => format!(
            "You cover {} only. To pass one line up to the chief overseer, run `herdr overseer escalate \"<line>\"`; use `--fyi` for plain status (situation file only), and `--wake` for decisions, money, sign-ins, taste or failures. Unclassified lines wake by default. You cannot reach other spaces.",
            space.map_or("this space".to_string(), |name| format!("the space \"{name}\""))
        ),
        Kind::Chief => "You are the chief overseer of every space: you get every event from spaces that have no overseer of their own, and the lines space overseers escalate. To message a space overseer, run `herdr overseer message <pane> \"<line>\"`.".to_string(),
    }
}

/// Active dispatcher rows with an output marker own their reports. A failed
/// read is not evidence of ownership, so direct task alarms remain enabled.
pub fn dispatched_stages(path: &std::path::Path) -> HashMap<String, PathBuf> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let fields: Vec<_> = line.split('\t').collect();
            if fields.len() > 8
                && matches!(fields[8], "sent" | "escalated")
                && !matches!(fields[5], "chore" | "decision")
                && !matches!(fields[4], "" | "-")
            {
                Some((fields[1].to_string(), PathBuf::from(fields[4])))
            } else {
                None
            }
        })
        .collect()
}

/// Recheck the actual hand-off, including markers written after the turn hook.
/// Marker existence alone is insufficient: an empty/partial file is a real stop.
pub fn task_end(marker: Option<&std::path::Path>, dispatcher_owned: bool) -> Option<&'static str> {
    if dispatcher_owned {
        return None;
    }
    let marker = marker?;
    let handoff = std::fs::read_to_string(marker)
        .is_ok_and(|text| text.lines().any(|line| line.trim() == "HANDOFF"));
    Some(if handoff {
        "finished"
    } else {
        "stopped without a hand-off"
    })
}

/// One plain line of at most `MAIL_CHARS` characters.
pub fn mail_line(text: &str) -> Result<String, (&'static str, &'static str)> {
    let text = text.trim();
    if text.is_empty() {
        return Err(("empty_text", "there is nothing to send"));
    }
    if text.contains(['\n', '\r']) {
        return Err(("one_line_only", "send one line"));
    }
    if text.chars().count() > MAIL_CHARS {
        return Err(("too_long", "the line is longer than 240 characters"));
    }
    Ok(text.chars().filter(|c| !c.is_control()).collect())
}

pub(crate) fn restore_input_guards(
    terminals: &HashMap<TerminalId, crate::terminal::TerminalState>,
    runtimes: &crate::terminal::TerminalRuntimeRegistry,
) {
    for (id, terminal) in terminals {
        if terminal.overseer.input_pending {
            if let Some(runtime) = runtimes.get(id) {
                runtime.record_input_bytes(b" ");
            }
        }
    }
}

pub const DEFAULT_NOTE: &str = "You are the overseer of this space. Before anything reaches the user, try every 'can't' yourself, check work against the current code, check tests ran on the real thing, and finish what the team can finish. The user only gets taste, money, and sign ins.";

pub fn expand_path(value: &str) -> PathBuf {
    value
        .strip_prefix("~/")
        .and_then(|tail| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(tail)))
        .unwrap_or_else(|| PathBuf::from(value))
}

/// Never include a prompt or its wrapped continuation. Unknown prompt shapes
/// fail closed. A transcript is untrusted data, not an instruction source.
pub fn output_only(screen: &str) -> Option<&str> {
    let mut offset = 0;
    let mut previous = 0;
    for line in screen.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with('›')
            || trimmed.starts_with('❯')
            || trimmed.starts_with("> ")
            || trimmed == ">"
        {
            let prev = screen[previous..offset].trim();
            let boundary = if !prev.is_empty()
                && prev
                    .chars()
                    .all(|c| matches!(c, '─' | '━' | ' ' | '╭' | '╮'))
            {
                previous
            } else {
                offset
            };
            return Some(&screen[..boundary]);
        }
        previous = offset;
        offset += line.len();
    }
    None
}

pub fn empty_prompt(screen: &str) -> bool {
    let mut offset = 0;
    let mut start = None;
    for line in screen.split_inclusive('\n') {
        let t = line.trim_start();
        if t.starts_with('›') || t.starts_with('❯') || t.starts_with("> ") || t.trim() == ">" {
            start = Some(offset);
        }
        offset += line.len();
    }
    let Some(start) = start else {
        return false;
    };
    let input = &screen[start..];
    let Some((_, prompt)) = input.lines().enumerate().find(|(_, line)| {
        let t = line.trim_start();
        t.starts_with('›') || t.starts_with('❯') || t.starts_with('>')
    }) else {
        return false;
    };
    // Placeholder prose can be typed text. Only a visibly empty prompt is safe.
    if !matches!(prompt.trim(), "›" | "❯" | ">") {
        return false;
    }
    let mut after = false;
    for line in input.lines() {
        if std::ptr::eq(line.as_ptr(), prompt.as_ptr()) {
            after = true;
            continue;
        }
        if after {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            if t.chars().all(|c| matches!(c, '─' | '━' | ' ' | '╰' | '╯')) {
                return true;
            }
            // Footer controls, never wrapped draft text.
            return t.contains("context left")
                || t.starts_with("? for shortcuts")
                || t.starts_with("ctrl+")
                || t.starts_with("esc ")
                || t.contains(" · ");
        }
    }
    true
}

/// Agents draw empty-input placeholder text faint. Preserve non-faint text,
/// including any typed draft, and retain the prompt marker itself.
pub fn empty_prompt_ansi(screen: &str, ansi: &str) -> bool {
    if empty_prompt(screen) {
        return true;
    }
    let mut faint = false;
    let mut cleaned = String::new();
    let mut chars = ansi.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            if chars.next() != Some('[') {
                return false;
            }
            let mut params = String::new();
            for ch in chars.by_ref() {
                if ch.is_ascii_alphabetic() {
                    if ch == 'm' {
                        let values: Vec<_> = params
                            .split(';')
                            .map(|v| v.parse::<u16>().unwrap_or(0))
                            .collect();
                        let mut i = 0;
                        while i < values.len() {
                            match values[i] {
                                38 | 48 | 58 => {
                                    i += match values.get(i + 1) {
                                        Some(2) => 4,
                                        Some(5) => 2,
                                        _ => 0,
                                    };
                                }
                                0 | 22 => faint = false,
                                2 => faint = true,
                                _ => {}
                            }
                            i += 1;
                        }
                    }
                    break;
                }
                params.push(ch);
            }
        } else if !faint || matches!(ch, '›' | '❯' | '>' | '\n' | '\r') {
            cleaned.push(ch);
        }
    }
    empty_prompt(&cleaned)
}

pub fn tail(screen: &str) -> String {
    let output = output_only(screen).unwrap_or("");
    let mut lines: Vec<_> = output
        .lines()
        .rev()
        .filter(|s| !s.trim().is_empty())
        .take(5)
        .collect();
    lines.reverse();
    lines
        .into_iter()
        .map(|line| {
            let lower = line.to_ascii_lowercase();
            if [
                "api_key",
                "api key",
                "bearer ",
                "password",
                "secret",
                "token=",
                "sk-",
                "authorization:",
                "cookie:",
            ]
            .iter()
            .any(|s| lower.contains(s))
            {
                "[sensitive line omitted]".to_string()
            } else {
                line.chars().filter(|c| !c.is_control()).take(240).collect()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn transition(
    previous: AgentState,
    current: AgentState,
    reason: Option<&str>,
    output: &str,
) -> Option<String> {
    if current == AgentState::Blocked && previous != AgentState::Blocked {
        Some(format!("blocked: {}", reason.unwrap_or("needs attention")))
    } else if previous == AgentState::Working && current == AgentState::Idle {
        Some("done".into())
    } else if previous != AgentState::Idle
        && current == AgentState::Idle
        && output
            .lines()
            .any(|line| line.trim_start().to_ascii_lowercase().starts_with("error:"))
    {
        Some("idle with an error".into())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prompt_and_wrapped_secrets_are_never_reported() {
        let screen = "Finished checks\n────────────────\n❯ password top-secret\nwrapped secret\n────────────────\n";
        assert_eq!(tail(screen), "Finished checks");
        assert!(!empty_prompt(screen));
        assert!(empty_prompt(
            "Finished\n────────────────\n❯ \n────────────────\n"
        ));
        assert!(!empty_prompt("Finished\n› \nwrapped draft\n"));
        assert_eq!(tail("shell input with secret"), "");
        assert_eq!(tail("api_key=private\n› \n"), "[sensitive line omitted]");
    }
    #[test]
    fn faint_placeholder_is_empty_but_colored_and_wrapped_drafts_are_not() {
        assert!(empty_prompt("› previous request\nFinished checks\n› \n"));
        assert!(empty_prompt_ansi(
            "done\n› Ask anything\n",
            "done\n› \x1b[2mAsk anything\x1b[0m\n"
        ));
        assert!(!empty_prompt_ansi(
            "done\n› typed draft\n",
            "done\n› \x1b[38;2;120;120;120mtyped draft\x1b[0m\n"
        ));
        assert!(!empty_prompt_ansi(
            "done\n› \nwrapped draft\n",
            "done\n› \nwrapped draft\n"
        ));
    }

    #[test]
    fn state_transitions_and_notice_serialization() {
        assert_eq!(
            transition(AgentState::Working, AgentState::Idle, None, ""),
            Some("done".into())
        );
        assert_eq!(
            transition(
                AgentState::Idle,
                AgentState::Blocked,
                Some("usage limit"),
                ""
            ),
            Some("blocked: usage limit".into())
        );
        assert_eq!(
            transition(AgentState::Blocked, AgentState::Blocked, None, ""),
            None
        );
        let mut role = Role::default();
        role.assign(Some(Kind::Chief));
        role.assign(Some(Kind::Chief));
        let saved: Role = serde_json::from_str(&serde_json::to_string(&role).unwrap()).unwrap();
        assert!(saved.active);
        assert_eq!(saved.notices.len(), 1);
        let mut role = Role {
            active: true,
            profile_base: Some(vec!["launcher".into()]),
            restart_argv: Some(vec!["resume".into()]),
            ..Role::default()
        };
        let live = role.clone().restored(true);
        assert!(live.pending_restart().is_some());
        let cold = role.clone().restored(false);
        assert!(cold.pending_restart().is_none());
        assert!(cold.notices.front().unwrap().silent);
        assert!(!cold.notices.front().unwrap().profile_checked);
        role.profile_base = None;
        assert!(role.restored(false).notices.is_empty());
    }
}

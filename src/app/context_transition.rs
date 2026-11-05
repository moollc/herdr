//! Context use of a role holder: ask for a hand-off past the limit, then run the
//! harness's own reset once the hand-off is valid for the live conversation, the
//! pane is idle and the input box is empty. Only the overseer tick calls this.
//! No model command, API method or effort change reaches it.
use std::time::{Duration, Instant};

use super::App;
use crate::api::schema::ManagedRole;
use crate::overseer::{ContextReport, TransitionTrack};
use crate::role_continuity::{self, ContextTransitionPlan, ContinuityRecord};
use crate::terminal::TerminalId;

/// A harness report older than this no longer describes the conversation.
const REPORT_MAX_AGE: Duration = Duration::from_secs(5 * 60);
/// One hand-off request per interval, so a holder that is slow to answer is not nagged.
const HANDOFF_REQUEST_INTERVAL: Duration = Duration::from_secs(30 * 60);

fn role_command(role: ManagedRole) -> &'static str {
    match role {
        ManagedRole::SpaceOverseer => "overseer",
        ManagedRole::SessionOverlord => "overlord",
    }
}

impl App {
    /// Entry point for a harness adapter: the holder's context use, bound to the
    /// native conversation it was read from. No adapter reports yet, so in a
    /// running session the meter stays unknown and nothing acts.
    #[allow(dead_code)] // adapters without a session id in their input, and tests
    pub(crate) fn report_context_usage(
        &mut self,
        id: &TerminalId,
        percent: u8,
        source: &str,
        now: Instant,
    ) -> bool {
        let Some(session) = self
            .state
            .terminals
            .get(id)
            .and_then(|terminal| terminal.current_session_identity_for_persistence())
        else {
            return false;
        };
        if percent > 100 {
            return false;
        }
        self.store_context_report(id, percent, source, session, now);
        true
    }

    fn store_context_report(
        &mut self,
        id: &TerminalId,
        percent: u8,
        source: &str,
        session: crate::overseer::SessionIdentity,
        now: Instant,
    ) {
        self.overseer_service.context_reports.insert(
            id.clone(),
            ContextReport {
                percent,
                source: source.to_string(),
                session,
                reported_at: now,
            },
        );
    }

    /// Drops the cached reading when it belongs to `session_id`. Another conversation's
    /// reading, or an unrelated metadata patch, is left alone.
    pub(crate) fn clear_context_report(
        &mut self,
        id: &TerminalId,
        session_id: Option<&str>,
    ) -> bool {
        let Some(session_id) = session_id.map(str::trim).filter(|value| !value.is_empty()) else {
            return false;
        };
        let owned = self
            .overseer_service
            .context_reports
            .get(id)
            .is_some_and(|report| {
                report.session.2 == crate::agent_resume::AgentSessionRefKind::Id
                    && report.session.3 == session_id
            });
        if owned {
            self.overseer_service.context_reports.remove(id);
        }
        owned
    }

    /// A reading that names its native conversation (the Claude status line input
    /// carries `session_id`). Kept only when the pane's live conversation is that one.
    pub(crate) fn report_context_from_session(
        &mut self,
        id: &TerminalId,
        percent: u8,
        session_id: Option<&str>,
        source: &str,
        now: Instant,
    ) -> bool {
        let Some(session_id) = session_id.map(str::trim).filter(|value| !value.is_empty()) else {
            return false;
        };
        if percent > 100 {
            return false;
        }
        let Some(session) = self
            .state
            .terminals
            .get(id)
            .and_then(|terminal| terminal.current_session_identity_for_persistence())
        else {
            return false;
        };
        if session.2 != crate::agent_resume::AgentSessionRefKind::Id || session.3 != session_id {
            return false;
        }
        self.store_context_report(id, percent, source, session, now);
        true
    }

    /// The reported figure when it is recent and belongs to the live conversation.
    pub(super) fn context_report(&self, id: &TerminalId, now: Instant) -> Option<&ContextReport> {
        let report = self.overseer_service.context_reports.get(id)?;
        let session = self
            .state
            .terminals
            .get(id)?
            .current_session_identity_for_persistence()?;
        (report.session == session && now.duration_since(report.reported_at) <= REPORT_MAX_AGE)
            .then_some(report)
    }

    fn held_roles(&self, id: &TerminalId) -> Vec<ManagedRole> {
        let Some(terminal) = self.state.terminals.get(id) else {
            return Vec::new();
        };
        let mut roles = Vec::new();
        if terminal.overseer.is_space_overseer() {
            roles.push(ManagedRole::SpaceOverseer);
        }
        if terminal.overseer.is_session_overlord() {
            roles.push(ManagedRole::SessionOverlord);
        }
        roles
    }

    /// Every role the pane holds needs a readable memory file whose newest
    /// hand-off passes the check for the live native conversation.
    fn role_handoffs_valid(&self, id: &TerminalId, now_unix: u64) -> Result<(), String> {
        let terminal = self
            .state
            .terminals
            .get(id)
            .ok_or_else(|| "terminal is unknown".to_string())?;
        let session = terminal.current_session_identity_for_persistence();
        let roles = self.held_roles(id);
        if roles.is_empty() {
            return Err("holds no managed role".into());
        }
        for role in roles {
            let (_, path) = self
                .role_memory_scope_path(id, role)
                .ok_or_else(|| "role scope is not available".to_string())?;
            let read = role_continuity::read_records_lenient(&path);
            if let Some(problem) = read.problems.first() {
                return Err(format!("memory unreadable: {problem}"));
            }
            let packet = read
                .records
                .iter()
                .rev()
                .find_map(|record| match record {
                    ContinuityRecord::Handoff { packet } => Some(packet),
                    _ => None,
                })
                .ok_or_else(|| "no hand-off recorded".to_string())?;
            let check = role_continuity::check_handoff(packet, now_unix, session.as_ref());
            if !check.valid {
                return Err(check.reason);
            }
        }
        Ok(())
    }

    fn pane_idle(&self, id: &TerminalId) -> bool {
        self.state.terminals.get(id).is_some_and(|terminal| {
            terminal.state == crate::detect::AgentState::Idle
                && terminal.effective_known_agent().is_some()
                && terminal.pending_restart.is_none()
                && terminal.overseer.notices.is_empty()
        })
    }

    fn native_action_supported(&self, id: &TerminalId) -> bool {
        let agent = self
            .state
            .terminals
            .get(id)
            .and_then(|terminal| terminal.effective_known_agent());
        matches!(
            role_continuity::adapter_lifecycle(agent),
            role_continuity::AdapterLifecycle::Supported(_)
        ) && agent.and_then(crate::detect::agent_reset_command).is_some()
    }

    fn context_plan(&self, id: &TerminalId, now: Instant) -> ContextTransitionPlan {
        let config = &self.overseer_service.config;
        role_continuity::context_transition_plan(
            config.context_transition_enabled,
            self.context_report(id, now).map(|report| report.percent),
            config.context_handoff_limit_percent,
            self.role_handoffs_valid(id, role_continuity::unix_now())
                .is_ok(),
            self.pane_idle(id),
            self.input_box_empty(id, now),
            self.native_action_supported(id),
        )
    }

    /// Card text for the meter and the transition state of one holder.
    pub(super) fn context_card(&self, id: &TerminalId, now: Instant) -> String {
        let meter = match self.context_report(id, now) {
            Some(report) => format!(
                "{}% ({}, age {}s)",
                report.percent,
                report.source,
                now.duration_since(report.reported_at).as_secs()
            ),
            None => "unknown".to_string(),
        };
        let config = &self.overseer_service.config;
        let transition = if !config.context_transition_enabled {
            "off by default".to_string()
        } else {
            match self.overseer_service.transitions.get(id) {
                Some(track) if !track.status.is_empty() => track.status.clone(),
                _ => "on, not yet evaluated".to_string(),
            }
        };
        format!(
            "context: {meter}; handoff threshold={}%; native transition={transition}",
            config.context_handoff_limit_percent
        )
    }

    fn set_transition_status(&mut self, id: &TerminalId, status: impl Into<String>) {
        self.overseer_service
            .transitions
            .entry(id.clone())
            .or_default()
            .status = status.into();
    }

    fn request_handoff(&mut self, id: &TerminalId, now: Instant) {
        let due = self
            .overseer_service
            .transitions
            .get(id)
            .and_then(|track| track.requested_at)
            .is_none_or(|at| now.duration_since(at) >= HANDOFF_REQUEST_INTERVAL);
        if !due {
            self.set_transition_status(id, "hand-off requested, waiting for a valid one");
            return;
        }
        if !self.overseer_ready(id, now) {
            self.set_transition_status(id, "hand-off request waits: pane busy or draft present");
            return;
        }
        let percent = self.context_report(id, now).map(|report| report.percent);
        let command = self
            .held_roles(id)
            .first()
            .map(|role| role_command(*role))
            .unwrap_or("overseer");
        let text = format!(
            "Context use is {}% of its window. Record a checked hand-off with `herdr {command} handoff record <task> <attempt> <candidate> <tests> <outputs> <findings or none> <cost> <next-owner>`. Herdr runs the harness's own reset after a valid hand-off and sends your role card again; do not clear yourself.",
            percent.unwrap_or_default()
        );
        if self.send_overseer(id, &text) {
            self.overseer_service
                .transitions
                .entry(id.clone())
                .or_default()
                .requested_at = Some(now);
            self.set_transition_status(id, "hand-off requested");
        } else {
            self.set_transition_status(id, "hand-off request could not be sent");
        }
    }

    /// Types the harness's own reset command, the way the user would. The pane
    /// is idle with an empty box, so no Escape is needed first.
    fn run_native_reset(&mut self, id: &TerminalId) -> Result<&'static str, String> {
        let agent = self
            .state
            .terminals
            .get(id)
            .and_then(|terminal| terminal.effective_known_agent())
            .ok_or_else(|| "harness is unknown".to_string())?;
        let command = crate::detect::agent_reset_command(agent)
            .ok_or_else(|| "harness has no reset command".to_string())?;
        let runtime = self
            .terminal_runtimes
            .get(id)
            .ok_or_else(|| "no live terminal".to_string())?;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let enter = runtime
            .encode_terminal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()).into());
        for bytes in [command.as_bytes().to_vec(), enter] {
            if runtime.try_send_bytes(bytes::Bytes::from(bytes)).is_err() {
                return Err("terminal rejected the input".into());
            }
        }
        Ok(command)
    }

    /// One evaluation per overseer tick. Does nothing unless the setting is on,
    /// a recent harness report exists and the figure is past the limit.
    pub(super) fn advance_context_transitions(&mut self, now: Instant) {
        if !self.overseer_service.config.context_transition_enabled {
            self.overseer_service.transitions.clear();
            return;
        }
        let holders: Vec<TerminalId> = self
            .state
            .terminals
            .iter()
            .filter(|(_, terminal)| {
                terminal.overseer.is_space_overseer() || terminal.overseer.is_session_overlord()
            })
            .map(|(id, _)| id.clone())
            .collect();
        self.overseer_service
            .transitions
            .retain(|id, _| holders.contains(id));
        for id in holders {
            match self.context_plan(&id, now) {
                ContextTransitionPlan::Disabled => {}
                ContextTransitionPlan::MeterUnknown => {
                    self.set_transition_status(&id, "waiting: context use unknown");
                }
                ContextTransitionPlan::BelowLimit => {
                    self.set_transition_status(&id, "below the limit");
                }
                ContextTransitionPlan::RequestHandoff => self.request_handoff(&id, now),
                ContextTransitionPlan::Blocked(reason) => {
                    self.set_transition_status(&id, format!("blocked: {reason}"));
                }
                ContextTransitionPlan::Unsupported(reason) => {
                    self.set_transition_status(&id, format!("unsupported: {reason}"));
                }
                ContextTransitionPlan::Ready => match self.run_native_reset(&id) {
                    Ok(command) => {
                        // The figure belongs to the conversation that was just reset.
                        self.overseer_service.context_reports.remove(&id);
                        self.overseer_service
                            .transitions
                            .insert(id.clone(), TransitionTrack::default());
                        self.set_transition_status(
                            &id,
                            format!("sent {command} after a valid hand-off"),
                        );
                    }
                    Err(reason) => {
                        self.set_transition_status(&id, format!("not sent: {reason}"));
                    }
                },
            }
        }
    }
}

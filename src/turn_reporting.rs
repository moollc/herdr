//! Explicit native turn boundaries. No prompt or response content is retained.
use crate::detect::Agent;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEvent {
    Ready,
    Started,
    Ended,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reporting {
    pub agent: String,
    pub working: bool,
    pub seq: u64,
    pub turn_id: Option<String>,
    pub ends: u64,
    pub finished: bool,
    pub saw_idle: bool,
}
impl Reporting {
    pub fn new(agent: Agent) -> Self {
        Self {
            agent: crate::detect::agent_label(agent).into(),
            working: false,
            seq: 0,
            turn_id: None,
            ends: 0,
            finished: false,
            saw_idle: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::AgentState;
    use crate::terminal::{TerminalId, TerminalState};
    use std::time::{Duration, Instant};
    fn pane(agent: Agent) -> TerminalState {
        let mut t = TerminalState::new(TerminalId::alloc(), std::env::temp_dir());
        t.detected_agent = Some(agent);
        t
    }
    #[test]
    fn native_turn_reporting_recovers_from_sustained_visible_idle_for_every_kind() {
        for agent in [Agent::Claude, Agent::Codex, Agent::Cursor] {
            let mut t = pane(agent);
            t.report_turn(agent, TurnEvent::Ready, 1, None);
            t.report_turn(agent, TurnEvent::Started, 2, None);
            let now = Instant::now();
            for seconds in [0, 3, 25, 120] {
                t.set_detected_state_with_reason_at(
                    Some(agent),
                    AgentState::Idle,
                    None,
                    false,
                    true,
                    false,
                    false,
                    now + Duration::from_secs(seconds),
                );
                let expected = if seconds < 15 {
                    AgentState::Working
                } else {
                    AgentState::Idle
                };
                assert_eq!(t.state, expected, "{agent:?} at {seconds}s");
            }
            t.report_turn(agent, TurnEvent::Ended, 3, Some("one".into()));
            assert_eq!(t.state, AgentState::Idle);
            assert_eq!(t.turn_reporting.as_ref().unwrap().ends, 1);
            assert!(t.report_turn(agent, TurnEvent::Ended, 2, None).is_none());
            assert!(t
                .report_turn(agent, TurnEvent::Ended, 4, Some("one".into()))
                .is_none());
            assert_eq!(t.turn_reporting.as_ref().unwrap().ends, 1);
        }
    }
    #[test]
    fn visible_idle_at_fifteen_seconds_recovers_without_native_completion() {
        for agent in [Agent::Claude, Agent::Codex, Agent::Cursor] {
            let mut t = pane(agent);
            t.report_turn(agent, TurnEvent::Started, 1, None);
            let now = Instant::now();
            for seconds in [0, 3, 14, 15, 25, 120] {
                t.set_detected_state_with_reason_at(
                    Some(agent),
                    AgentState::Idle,
                    None,
                    false,
                    true,
                    false,
                    false,
                    now + Duration::from_secs(seconds),
                );
                assert_eq!(
                    t.state,
                    if seconds < 15 {
                        AgentState::Working
                    } else {
                        AgentState::Idle
                    }
                );
                let native = t.turn_reporting.as_ref().unwrap();
                assert_eq!(native.ends, 0);
                assert!(!native.finished);
            }
        }
    }
    #[test]
    fn native_turn_reporting_keeps_blockers_errors_and_process_exit() {
        for agent in [Agent::Claude, Agent::Codex, Agent::Cursor] {
            let mut t = pane(agent);
            t.report_turn(agent, TurnEvent::Started, 1, None);
            t.set_detected_state_with_reason_at(
                Some(agent),
                AgentState::Blocked,
                Some("approval"),
                true,
                false,
                false,
                false,
                Instant::now(),
            );
            assert_eq!(t.state, AgentState::Blocked);
            t.report_turn(agent, TurnEvent::Ended, 2, None);
            assert_eq!(t.state, AgentState::Blocked);
            t.set_detected_state_with_reason_at(
                Some(agent),
                AgentState::Idle,
                None,
                false,
                true,
                false,
                false,
                Instant::now(),
            );
            assert_eq!(t.state, AgentState::Idle);
            t.set_detected_state_with_reason_at(
                Some(agent),
                AgentState::Working,
                None,
                false,
                false,
                true,
                false,
                Instant::now(),
            );
            assert_eq!(t.state, AgentState::Working);
            t.set_detected_state_with_reason_at(
                Some(agent),
                AgentState::Blocked,
                Some("agent exited"),
                true,
                false,
                false,
                true,
                Instant::now(),
            );
            assert!(t.turn_reporting.is_none());
            assert_eq!(t.state, AgentState::Blocked);
        }
    }
    #[test]
    fn native_turn_reporting_samples_marker_only_at_the_end() {
        let mut t = pane(Agent::Codex);
        let folder = std::env::temp_dir().join(format!("turn-marker-{}", t.id));
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("job.done");
        t.job_marker = Some(path.clone());
        t.report_turn(Agent::Codex, TurnEvent::Ended, 1, None);
        assert!(!t.turn_reporting.as_ref().unwrap().finished);
        std::fs::write(&path, "HANDOFF").unwrap();
        assert!(!t.turn_reporting.as_ref().unwrap().finished);
        t.report_turn(Agent::Codex, TurnEvent::Ended, 2, None);
        assert!(t.turn_reporting.as_ref().unwrap().finished);
    }
    #[test]
    fn native_turn_reporting_screen_fallback_and_stale_spinner() {
        let mut t = pane(Agent::Codex);
        t.set_detected_state_with_reason_at(
            Some(Agent::Codex),
            AgentState::Idle,
            None,
            false,
            true,
            false,
            false,
            Instant::now(),
        );
        assert_eq!(t.state, AgentState::Idle);
        t.report_turn(Agent::Codex, TurnEvent::Ended, 1, None);
        t.set_detected_state_with_reason_at(
            Some(Agent::Codex),
            AgentState::Working,
            None,
            false,
            false,
            true,
            false,
            Instant::now(),
        );
        assert_eq!(t.state, AgentState::Idle);
        assert!(t
            .report_turn(Agent::Claude, TurnEvent::Ended, 2, None)
            .is_none());
    }
    #[test]
    fn interrupted_ready_prompt_recovers_without_end_hook_and_rearms() {
        let content = include_str!("../tests/fixtures/interrupted-ready.txt");
        let detection = crate::detect::detect_agent(Some(Agent::Claude), content);
        assert!(detection.visible_idle && !detection.visible_working && !detection.visible_blocker);
        for agent in [Agent::Claude, Agent::Codex, Agent::Cursor] {
            let mut t = pane(agent);
            t.report_turn(agent, TurnEvent::Started, 1, None);
            let now = Instant::now();
            for seconds in [0, 14, 15] {
                t.set_detected_state_with_reason_at(
                    Some(agent),
                    AgentState::Idle,
                    None,
                    false,
                    true,
                    false,
                    false,
                    now + Duration::from_secs(seconds),
                );
                assert_eq!(
                    t.state,
                    if seconds < 15 {
                        AgentState::Working
                    } else {
                        AgentState::Idle
                    }
                );
            }
            assert_eq!(
                t.turn_reporting.as_ref().unwrap().ends,
                0,
                "recovery is not a fabricated completion"
            );
            assert!(!t.turn_reporting.as_ref().unwrap().finished);
            t.report_turn(agent, TurnEvent::Started, 2, None);
            assert_eq!(t.state, AgentState::Working);
        }
    }
    #[test]
    fn positive_idle_recovery_resets_on_ambiguous_or_working_frame() {
        let mut t = pane(Agent::Claude);
        t.report_turn(Agent::Claude, TurnEvent::Started, 1, None);
        let now = Instant::now();
        t.set_detected_state_with_reason_at(
            Some(Agent::Claude),
            AgentState::Idle,
            None,
            false,
            true,
            false,
            false,
            now,
        );
        t.set_detected_state_with_reason_at(
            Some(Agent::Claude),
            AgentState::Working,
            None,
            false,
            false,
            true,
            false,
            now + Duration::from_secs(14),
        );
        t.set_detected_state_with_reason_at(
            Some(Agent::Claude),
            AgentState::Idle,
            None,
            false,
            true,
            false,
            false,
            now + Duration::from_secs(15),
        );
        assert_eq!(t.state, AgentState::Working);
        t.set_detected_state_with_reason_at(
            Some(Agent::Claude),
            AgentState::Idle,
            None,
            false,
            false,
            false,
            false,
            now + Duration::from_secs(29),
        );
        assert_eq!(t.state, AgentState::Working);
    }
    #[test]
    fn accepted_manual_idle_overrides_native_working_but_stale_report_does_not() {
        for agent in [Agent::Claude, Agent::Codex, Agent::Cursor] {
            let mut t = pane(agent);
            t.report_turn(agent, TurnEvent::Started, 1, None);
            t.set_hook_authority_with_custom_status_at(
                "manual".into(),
                crate::detect::agent_label(agent).into(),
                AgentState::Idle,
                None,
                None,
                None,
                Some(100),
                Instant::now(),
            )
            .unwrap();
            assert_eq!(t.state, AgentState::Idle);
            t.report_turn(agent, TurnEvent::Started, 2, None);
            assert!(t
                .set_hook_authority_with_custom_status_at(
                    "manual".into(),
                    crate::detect::agent_label(agent).into(),
                    AgentState::Idle,
                    None,
                    None,
                    None,
                    Some(99),
                    Instant::now()
                )
                .is_none());
            assert_eq!(t.state, AgentState::Working);
        }
    }
}

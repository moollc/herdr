//! Agent deletion is distinct from closing a pane, which sets its agent down.
use crate::api::schema::{AgentCloseCandidate, AgentCloseParams, AgentCloseResult, ResponseResult};
use crate::app::App;
use crate::detect::AgentState;

use super::responses::{encode_error, encode_error_body, encode_success};

impl App {
    pub(super) fn handle_agent_close(&mut self, id: String, params: AgentCloseParams) -> String {
        if params.target.is_none() && !params.detached {
            return encode_error(id, "invalid_params", "provide a target or --detached");
        }
        // An unresolvable identity must never fall through to operator scope.
        let caller = match params.caller_pane_id.as_deref() {
            Some(raw) => {
                let Some(terminal_id) = self
                    .parse_reporting_pane_id(raw)
                    .and_then(|pane| self.state.terminal_id_for_any_pane(pane))
                else {
                    return encode_error(id, "permission_denied", "calling pane not found");
                };
                let Some(terminal) = self.state.terminals.get(&terminal_id) else {
                    return encode_error(id, "permission_denied", "calling terminal not found");
                };
                if !terminal.overseer.is_session_overlord()
                    && !terminal.overseer.is_space_overseer()
                {
                    return encode_error(id, "permission_denied", "only a space overseer, session overlord, or external CLI operator may close agents");
                }
                Some((terminal_id, terminal.overseer.is_session_overlord()))
            }
            None => None,
        };
        let space_only = caller.as_ref().is_some_and(|(_, chief)| !chief);
        let caller_space = caller
            .as_ref()
            .and_then(|(terminal, _)| self.state.terminal_places().get(terminal).copied());
        if space_only && params.detached && !params.allow_detached {
            return encode_error(id, "permission_denied", "space overseer cannot close detached agents without --allow-detached (or session overlord role)");
        }
        let agents = if let Some(target) = params.target.as_deref() {
            match self.agent_info_for_target(target) {
                Ok(agent) => vec![agent],
                Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
            }
        } else {
            self.state
                .detached_agents
                .iter()
                .filter_map(|item| self.detached_agent_info(item))
                .collect()
        };
        let mut candidates = Vec::new();
        let mut selected = Vec::new();
        // Complete authorization and safety evaluation before removing any pane:
        // closing its last pane can change every subsequent workspace index.
        for agent in agents {
            let Some(pane) = self
                .resolve_terminal_target(&agent.terminal_id)
                .ok()
                .map(|r| r.pane_id)
            else {
                return encode_error(id, "agent_not_found", "candidate disappeared");
            };
            let Some(terminal_id) = self.state.terminal_id_for_any_pane(pane) else {
                return encode_error(id, "agent_not_found", "candidate terminal disappeared");
            };
            let Some(terminal) = self.state.terminals.get(&terminal_id) else {
                return encode_error(id, "agent_not_found", "candidate terminal disappeared");
            };
            let detached = self.state.is_detached_agent_pane(pane);
            let target_space = self.state.workspace_index_for_pane(pane);
            if space_only {
                if detached && !params.allow_detached {
                    return encode_error(id, "permission_denied", "space overseer cannot close detached agents without --allow-detached (or session overlord role)");
                }
                if !detached && (caller_space.is_none() || target_space != caller_space) {
                    return encode_error(
                        id,
                        "permission_denied",
                        "space overseer may only close agents in its own space",
                    );
                }
            }
            let reason = if caller
                .as_ref()
                .is_some_and(|(caller, _)| *caller == terminal_id)
            {
                Some("cannot_close_caller")
            } else if terminal.overseer.is_space_overseer()
                || terminal.overseer.is_session_overlord()
            {
                Some("active_overseer_or_overlord")
            } else if params.detached && !detached {
                Some("not_detached")
            } else if !params.force && terminal.state == AgentState::Working {
                Some("agent_working")
            } else if !params.force && terminal.state == AgentState::Blocked {
                Some("agent_blocked")
            } else if params.idle_over_seconds.is_some_and(|minimum| {
                terminal.state != AgentState::Idle
                    || agent.idle_seconds.is_none_or(|idle| idle < minimum)
            }) {
                Some("idle_under_threshold")
            } else {
                None
            };
            if params.apply && params.target.is_some() {
                if let Some(reason) = reason {
                    match reason {
                        "cannot_close_caller" | "active_overseer_or_overlord" => return encode_error(id, reason, "release the role before closing this agent; the caller can never close itself"),
                        "agent_working" | "agent_blocked" => return encode_error(id, "agent_busy", "agent is working/blocked; pass --force to close anyway"),
                        _ => {}
                    }
                }
            }
            if reason.is_none() {
                selected.push(pane);
            }
            candidates.push(AgentCloseCandidate {
                terminal_id: agent.terminal_id,
                raw_pane_id: format!("p_{}", pane.raw()),
                pane_id: (!detached).then_some(agent.pane_id),
                workspace_id: (!detached).then_some(agent.workspace_id.clone()),
                location: if detached {
                    "detached".into()
                } else {
                    agent.workspace_id
                },
                agent: agent.agent.unwrap_or_else(|| "unknown".into()),
                agent_status: format!("{:?}", agent.agent_status).to_lowercase(),
                idle_seconds: agent.idle_seconds,
                title: agent.title.unwrap_or_default(),
                cwd: agent.cwd.unwrap_or_default(),
                action: if reason.is_some() {
                    "skipped"
                } else {
                    "would_close"
                }
                .into(),
                reason: reason.map(str::to_string),
            });
        }
        let mut closed_count = 0;
        if params.apply {
            for pane in selected {
                if self
                    .state
                    .close_agent_in_pane(&self.terminal_runtimes, pane)
                {
                    closed_count += 1;
                    if let Some(candidate) = candidates
                        .iter_mut()
                        .find(|c| c.raw_pane_id == format!("p_{}", pane.raw()))
                    {
                        candidate.action = "closed".into();
                    }
                }
            }
            self.shutdown_detached_terminal_runtimes();
            if closed_count > 0 {
                let audit = self
                    .log_agent_close_to_situation(params.caller_pane_id.as_deref(), &candidates);
                self.save_session_now();
                if let Err(error) = audit {
                    tracing::error!(%error, closed_count, "agents closed but audit write failed");
                    return encode_error(id, "agent_close_audit_failed", format!("closed {closed_count} agent(s), but could not persist the audit: {error}"));
                }
            }
        }
        encode_success(
            id,
            ResponseResult::AgentClose(AgentCloseResult {
                dry_run: !params.apply,
                closed_count,
                would_close_count: candidates
                    .iter()
                    .filter(|c| c.action == "would_close")
                    .count(),
                skipped_count: candidates.iter().filter(|c| c.action == "skipped").count(),
                candidates,
            }),
        )
    }
}

#[cfg(test)]
#[path = "agent_close_tests.rs"]
mod tests;

//! Native turn reports and pane-scoped dispatcher hand-offs.
use super::responses::{encode_error, encode_success};
use crate::api::schema::{
    PaneJobMarkerParams, PaneReportTurnParams, PaneSetJobMarkerParams, ResponseResult,
};
use crate::app::App;
impl App {
    pub(super) fn handle_pane_report_turn(
        &mut self,
        id: String,
        params: PaneReportTurnParams,
    ) -> String {
        let Some(pane_id) = self.parse_reporting_pane_id(&params.pane_id) else {
            return encode_error(id, "pane_not_found", "reporting pane not found");
        };
        let agent = match crate::detect::parse_agent_label(&params.agent) {
            Some(
                agent @ (crate::detect::Agent::Claude
                | crate::detect::Agent::Codex
                | crate::detect::Agent::Cursor),
            ) => agent,
            _ => return encode_error(id, "invalid_agent", "unsupported turn reporter"),
        };
        tracing::debug!(?agent, event = ?params.event, ?pane_id, "native turn report");
        self.handle_internal_event(crate::events::AppEvent::TurnReported {
            pane_id,
            agent,
            event: params.event,
            seq: params.seq,
            turn_id: params.turn_id,
        });
        encode_success(id, ResponseResult::Ok {})
    }
    pub(super) fn handle_pane_set_job_marker(
        &mut self,
        id: String,
        params: PaneSetJobMarkerParams,
    ) -> String {
        let Some(pane) = self.parse_reporting_pane_id(&params.pane_id) else {
            return encode_error(id, "pane_not_found", "pane not found");
        };
        let path = params.path.map(std::path::PathBuf::from);
        if path.as_ref().is_some_and(|p| !p.is_absolute()) {
            return encode_error(id, "invalid_marker", "marker path must be absolute");
        }
        let Some(terminal) = self
            .state
            .terminal_id_for_any_pane(pane)
            .and_then(|tid| self.state.terminals.get_mut(&tid))
        else {
            return encode_error(id, "pane_not_found", "terminal not found");
        };
        terminal.job_marker = path;
        let terminal_id = terminal.id.clone();
        self.overseer_service.direct_stops.remove(&terminal_id);
        let turn_key = format!("{terminal_id}:turn");
        for holder in self.overseer_service.holders.values_mut() {
            holder.events.remove(&turn_key);
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();
        encode_success(id, ResponseResult::Ok {})
    }
    pub(super) fn handle_pane_job_marker(
        &mut self,
        id: String,
        params: PaneJobMarkerParams,
    ) -> String {
        let pane = self.parse_reporting_pane_id(&params.pane_id);
        if pane.is_none() || pane != self.parse_reporting_pane_id(&params.caller_pane_id) {
            return encode_error(
                id,
                "own_pane_required",
                "job markers are readable only with matching caller pane context",
            );
        }
        let path = pane
            .and_then(|p| self.state.terminal_id_for_any_pane(p))
            .and_then(|tid| self.state.terminals.get(&tid))
            .and_then(|t| t.job_marker.as_ref())
            .map(|p| p.display().to_string());
        encode_success(id, ResponseResult::JobMarker { path })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn app_with_two_panes() -> (App, crate::layout::PaneId) {
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        app.state = crate::app::AppState::test_new();
        let ws = crate::workspace::Workspace::test_new("test");
        let pane = ws.tabs[0].root_pane;
        app.state.workspaces.push(ws);
        app.state.ensure_test_terminals();
        (app, pane)
    }
    #[tokio::test]
    async fn native_turn_marker_api_is_scoped_and_read_only() {
        let (mut app, pane) = app_with_two_panes();
        let pane_id = format!("p_{}", pane.raw());
        let before = app.state.terminal_id_for_any_pane(pane).unwrap();
        assert!(app
            .handle_pane_set_job_marker(
                "set".into(),
                PaneSetJobMarkerParams {
                    pane_id: pane_id.clone(),
                    path: Some("relative.done".into()),
                }
            )
            .contains("invalid_marker"));
        assert!(app
            .handle_pane_set_job_marker(
                "set".into(),
                PaneSetJobMarkerParams {
                    pane_id: pane_id.clone(),
                    path: Some("/tmp/job.done".into()),
                }
            )
            .contains("ok"));
        // Explicit next-stage registration resets dedupe even if a caller
        // intentionally reuses its marker path.
        let now = std::time::Instant::now();
        let mut stop = crate::overseer::DirectStop::new("/tmp/job.done".into(), 1);
        assert!(!stop.observe(1, true, true, now));
        assert!(stop.observe(1, true, false, now + std::time::Duration::from_secs(120)));
        app.overseer_service
            .direct_stops
            .insert(before.clone(), stop);
        let mut holder = crate::overseer::Holder::new(crate::overseer::Kind::Chief, now);
        holder.events.insert(
            format!("{before}:turn"),
            crate::overseer::PendingEvent {
                text: "worker stopped without a hand-off".into(),
                state: crate::detect::AgentState::Idle,
                reason: None,
                since: now,
                marker: Some("/tmp/job.done".into()),
            },
        );
        app.overseer_service.holders.insert(before.clone(), holder);
        assert!(app
            .handle_pane_set_job_marker(
                "next".into(),
                PaneSetJobMarkerParams {
                    pane_id: pane_id.clone(),
                    path: Some("/tmp/job.done".into()),
                }
            )
            .contains("ok"));
        assert!(!app.overseer_service.direct_stops.contains_key(&before));
        assert!(app.overseer_service.events_empty());
        assert!(app
            .handle_pane_job_marker(
                "get".into(),
                PaneJobMarkerParams {
                    pane_id: pane_id.clone(),
                    caller_pane_id: "p_999999".into(),
                }
            )
            .contains("own_pane_required"));
        let got = app.handle_pane_job_marker(
            "get".into(),
            PaneJobMarkerParams {
                pane_id: pane_id.clone(),
                caller_pane_id: pane_id.clone(),
            },
        );
        assert!(got.contains("/tmp/job.done"));
        assert_eq!(
            app.state.terminals[&before].job_marker.as_deref(),
            Some(std::path::Path::new("/tmp/job.done"))
        );
        app.handle_pane_set_job_marker(
            "clear".into(),
            PaneSetJobMarkerParams {
                pane_id,
                path: None,
            },
        );
        assert!(app.state.terminals[&before].job_marker.is_none());
    }
}

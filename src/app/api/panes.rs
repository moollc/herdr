use bytes::Bytes;

use crate::api::schema::{
    EventData, EventEnvelope, EventKind, PaneAckOrientationParams, PaneBindReportingAliasParams,
    PaneClearAgentAuthorityParams, PaneListParams, PanePinModelParams, PaneReadParams,
    PaneReadResult, PaneReleaseAgentParams, PaneRenameParams, PaneReportAgentParams,
    PaneReportAgentSessionParams, PaneReportMetadataParams, PaneSendInputParams,
    PaneSendKeysParams, PaneSendTextParams, PaneSplitParams, PaneTarget, ReadFormat, ReadSource,
    ResponseResult,
};
use crate::app::{App, Mode};

use super::super::api_helpers::{
    deliver_text_then_keys, detect_state_from_api, encode_api_keys, encode_api_text,
    normalize_custom_status, normalize_reported_agent_label,
};
use super::responses::{encode_error, encode_success};

impl App {
    pub(super) fn handle_pane_split(&mut self, id: String, params: PaneSplitParams) -> String {
        let Some((ws_idx, target_pane_id)) = self.parse_pane_id(&params.target_pane_id) else {
            return pane_not_found(id, &params.target_pane_id);
        };
        let (rows, cols) = self.state.estimate_pane_size();
        let split_cwd = params.cwd.map(std::path::PathBuf::from).or_else(|| {
            let follow_cwd = self.state.workspaces.get(ws_idx).and_then(|ws| {
                let tab_idx = ws.find_tab_index_for_pane(target_pane_id)?;
                ws.tabs.get(tab_idx)?.cwd_for_pane(
                    target_pane_id,
                    &self.state.terminals,
                    &self.terminal_runtimes,
                )
            });
            Some(self.resolve_new_terminal_cwd(follow_cwd))
        });
        let default_shell = self.state.default_shell.clone();
        let scrollback_limit_bytes = self.state.pane_scrollback_limit_bytes;
        let host_terminal_theme = self.state.host_terminal_theme;
        let previous_focus = self.state.current_pane_focus_target();
        let Some(ws) = self.state.workspaces.get_mut(ws_idx) else {
            return pane_not_found(id, &params.target_pane_id);
        };
        let direction = match params.direction {
            crate::api::schema::SplitDirection::Right => ratatui::layout::Direction::Horizontal,
            crate::api::schema::SplitDirection::Down => ratatui::layout::Direction::Vertical,
        };
        let (target_tab_idx, new_pane) = match ws.split_pane(
            target_pane_id,
            direction,
            rows,
            cols,
            split_cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            crate::pane::PaneShellConfig::new(&default_shell, self.state.shell_mode),
            params.focus,
        ) {
            Some(Ok(result)) => result,
            Some(Err(err)) => return encode_error(id, "pane_split_failed", err.to_string()),
            None => return pane_not_found(id, &params.target_pane_id),
        };
        if params.focus {
            self.state.switch_workspace_tab(ws_idx, target_tab_idx);
            self.state
                .record_pane_focus_change(previous_focus, ws_idx, new_pane.pane_id);
            self.state.mode = Mode::Terminal;
        }
        self.terminal_runtimes
            .insert(new_pane.terminal.id.clone(), new_pane.runtime);
        self.state
            .remove_alias_shadowed_by_new_pane(new_pane.pane_id);
        self.state
            .terminals
            .insert(new_pane.terminal.id.clone(), new_pane.terminal);
        self.schedule_session_save();
        let pane = self.pane_info(ws_idx, new_pane.pane_id).unwrap();
        self.emit_event(EventEnvelope {
            event: EventKind::PaneCreated,
            data: EventData::PaneCreated {
                pane: Box::new(pane.clone()),
            },
        });

        encode_success(id, ResponseResult::PaneInfo { pane })
    }

    pub(super) fn handle_pane_list(&mut self, id: String, params: PaneListParams) -> String {
        match self.collect_panes_for_workspace(params.workspace_id.as_deref()) {
            Ok(panes) => encode_success(id, ResponseResult::PaneList { panes }),
            Err((code, message)) => encode_error(id, &code, message),
        }
    }

    pub(super) fn handle_pane_get(&mut self, id: String, target: PaneTarget) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&target.pane_id) else {
            return pane_not_found(id, &target.pane_id);
        };
        let Some(pane) = self.pane_info(ws_idx, pane_id) else {
            return pane_not_found(id, &target.pane_id);
        };

        encode_success(id, ResponseResult::PaneInfo { pane })
    }

    pub(super) fn handle_pane_context(&mut self, id: String, target: PaneTarget) -> String {
        let Some((_, pane_id)) = self.parse_pane_id(&target.pane_id) else {
            return pane_not_found(id, &target.pane_id);
        };
        let Some(terminal_id) = self.state.terminal_id_for_any_pane(pane_id) else {
            return pane_not_found(id, &target.pane_id);
        };
        encode_success(
            id,
            ResponseResult::PaneContext {
                terminal_id: terminal_id.to_string(),
                state_dir: crate::config::state_dir().display().to_string(),
                session_name: crate::session::active_name()
                    .unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.to_string()),
            },
        )
    }

    pub(super) fn handle_pane_bind_reporting_alias(
        &mut self,
        id: String,
        params: PaneBindReportingAliasParams,
    ) -> String {
        let Some(raw) = params
            .raw_pane_id
            .strip_prefix("p_")
            .and_then(|value| value.parse::<u32>().ok())
        else {
            return encode_error(
                id,
                "invalid_reporting_alias",
                "raw pane id must be p_<number>".to_string(),
            );
        };
        if params.raw_pane_id != format!("p_{raw}") {
            return encode_error(
                id,
                "invalid_reporting_alias",
                "raw pane id must use its canonical decimal form".to_string(),
            );
        }
        let target = self.state.terminals.keys().find_map(|terminal| {
            if terminal.to_string() != params.terminal_id {
                return None;
            }
            let pane_id = self
                .state
                .workspaces
                .iter()
                .find_map(|workspace| {
                    workspace.tabs.iter().find_map(|tab| {
                        tab.panes.iter().find_map(|(pane_id, pane)| {
                            (pane.attached_terminal_id == *terminal).then_some(*pane_id)
                        })
                    })
                })
                .or_else(|| {
                    self.state.detached_agents.iter().find_map(|agent| {
                        (agent.pane.attached_terminal_id == *terminal).then_some(agent.pane_id)
                    })
                })?;
            Some((pane_id, terminal.to_string()))
        });
        let Some((target_pane, target_terminal)) = target else {
            return encode_error(
                id,
                "terminal_not_found",
                "terminal id is not present in this Herdr session".to_string(),
            );
        };
        let before_terminal_id = self
            .state
            .pane_id_aliases
            .get(&raw)
            .copied()
            .or_else(|| {
                let direct = crate::layout::PaneId::from_raw(raw);
                self.state.terminal_id_for_any_pane(direct).map(|_| direct)
            })
            .and_then(|pane| self.state.terminal_id_for_any_pane(pane))
            .map(|terminal| terminal.to_string());
        if params.apply {
            self.state.pane_id_aliases.insert(raw, target_pane);
            self.schedule_session_save();
        }
        encode_success(
            id,
            ResponseResult::PaneReportingAlias {
                raw_pane_id: format!("p_{raw}"),
                before_terminal_id,
                after_terminal_id: target_terminal,
                applied: params.apply,
            },
        )
    }

    pub(super) fn handle_pane_rename(&mut self, id: String, params: PaneRenameParams) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.terminal_id(pane_id))
            .cloned()
        else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(terminal) = self.state.terminals.get_mut(&terminal_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        match params.label.map(|label| label.trim().to_string()) {
            Some(label) if !label.is_empty() => terminal.set_manual_label(label),
            _ => terminal.clear_manual_label(),
        }
        self.state.mark_session_dirty();
        let pane = self.pane_info(ws_idx, pane_id).unwrap();

        encode_success(id, ResponseResult::PaneInfo { pane })
    }

    pub(super) fn handle_pane_pin_model(
        &mut self,
        id: String,
        params: PanePinModelParams,
    ) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let pin = match params.model.as_deref() {
            None => {
                if params.effort.is_some() || params.launcher.is_some() || !params.args.is_empty() {
                    return encode_error(
                        id,
                        "invalid_model_pin",
                        "an effort, launcher or flags need a model".to_string(),
                    );
                }
                None
            }
            Some(model) => match crate::agent_model::ModelPin::new(model, params.effort.as_deref())
                .and_then(|pin| pin.with_launch(params.launcher.as_deref(), &params.args))
            {
                Some(pin) => Some(pin),
                None => {
                    return encode_error(
                        id,
                        "invalid_model_pin",
                        "model must not be empty".to_string(),
                    )
                }
            },
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.terminal_id(pane_id))
            .cloned()
        else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(terminal) = self.state.terminals.get_mut(&terminal_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        // A waiting resume and a saved launch command stay unpinned; the pin
        // is applied when they launch, so setting or clearing it needs no
        // rewrite here.
        terminal.model_pin = pin;
        self.state.mark_session_dirty();
        self.schedule_session_save();
        let pane = self.pane_info(ws_idx, pane_id).unwrap();

        encode_success(id, ResponseResult::PaneInfo { pane })
    }

    pub(super) fn handle_pane_read(&mut self, id: String, params: PaneReadParams) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some((pane, workspace_id)) = self.lookup_runtime(ws_idx, pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(tab_idx) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.find_tab_index_for_pane(pane_id))
        else {
            return pane_not_found(id, &params.pane_id);
        };
        let requested_lines = params.lines.unwrap_or(80).min(1000) as usize;
        let text = match params.format {
            ReadFormat::Text => match params.source {
                ReadSource::Visible => pane.visible_text(),
                ReadSource::Recent => pane.recent_text(requested_lines),
                ReadSource::RecentUnwrapped => pane.recent_unwrapped_text(requested_lines),
            },
            ReadFormat::Ansi => match params.source {
                ReadSource::Visible => pane.visible_ansi(),
                ReadSource::Recent => pane.recent_ansi(requested_lines),
                ReadSource::RecentUnwrapped => pane.recent_unwrapped_ansi(requested_lines),
            },
        };

        encode_success(
            id,
            ResponseResult::PaneRead {
                read: PaneReadResult {
                    pane_id: params.pane_id,
                    workspace_id,
                    tab_id: self.public_tab_id(ws_idx, tab_idx).unwrap(),
                    source: params.source,
                    format: params.format,
                    text,
                    revision: 0,
                    truncated: false,
                },
            },
        )
    }

    pub(super) fn handle_pane_report_agent(
        &mut self,
        id: String,
        params: PaneReportAgentParams,
    ) -> String {
        let Some(pane_id) = self.parse_reporting_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(agent_label) = normalize_reported_agent_label(&params.agent) else {
            return invalid_agent(id);
        };
        self.handle_internal_event(crate::events::AppEvent::HookStateReported {
            pane_id,
            session_ref: crate::agent_resume::session_ref_from_report(
                &params.source,
                &agent_label,
                params.agent_session_id,
                params.agent_session_path,
            ),
            source: params.source,
            agent_label,
            state: detect_state_from_api(params.state),
            message: params.message,
            custom_status: normalize_custom_status(params.custom_status),
            seq: params.seq,
        });

        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_pane_report_agent_session(
        &mut self,
        id: String,
        params: PaneReportAgentSessionParams,
    ) -> String {
        let Some(pane_id) = self.parse_reporting_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(agent_label) = normalize_reported_agent_label(&params.agent) else {
            return invalid_agent(id);
        };
        let terminal_id = self.state.terminal_id_for_any_pane(pane_id);
        let previous = terminal_id
            .as_ref()
            .and_then(|terminal_id| self.state.terminals.get(terminal_id))
            .map(|terminal| {
                (
                    terminal.persisted_agent_session.clone(),
                    terminal.orientation,
                )
            });
        let session_id = params.agent_session_id.clone();
        let startup_source = params.startup_source.clone();
        let native_source = params.source.clone();
        let native_context_hook = matches!(
            (native_source.as_str(), agent_label.as_str()),
            ("herdr:codex", "codex") | ("herdr:claude", "claude")
        );
        self.handle_internal_event(crate::events::AppEvent::AgentSessionReported {
            pane_id,
            session_ref: crate::agent_resume::session_ref_from_report(
                &params.source,
                &agent_label,
                params.agent_session_id,
                params.agent_session_path,
            ),
            source: native_source.clone(),
            agent_label: agent_label.clone(),
            seq: params.seq,
        });

        if params.turn_reporting {
            if let Some(agent) = crate::detect::parse_agent_label(&agent_label) {
                self.handle_internal_event(crate::events::AppEvent::TurnReported {
                    pane_id,
                    agent,
                    event: crate::turn_reporting::TurnEvent::Ready,
                    seq: params.seq.unwrap_or(1),
                    turn_id: None,
                });
            }
        }
        // SessionStart is a context channel, independent of literal task input.
        // Native lifecycle hooks can refresh the card only when their exact
        // conversation identity still matches Herdr's persisted session.
        let lifecycle_refresh = matches!(
            startup_source.as_deref(),
            Some("clear" | "compact" | "resume")
        );
        let startup_refresh = startup_source.as_deref() == Some("startup");
        if native_context_hook
            && (startup_source.as_deref() == Some("user_prompt_submit")
                || lifecycle_refresh
                || startup_refresh)
        {
            if let Some(terminal_id) = terminal_id.as_ref() {
                let accepted_session = self
                    .state
                    .terminals
                    .get(terminal_id)
                    .and_then(|terminal| terminal.persisted_agent_session.as_ref())
                    .filter(|session| {
                        session.source == native_source
                            && session.agent == agent_label
                            && Some(session.session_ref.value.as_str()) == session_id.as_deref()
                    });
                if accepted_session.is_some() {
                    let already_delivered_startup = startup_refresh
                        && previous
                            .as_ref()
                            .is_some_and(|(old_session, old_orientation)| {
                                *old_orientation
                                    == crate::agent_orientation::OrientationState::Delivered
                                    && (old_session.is_none()
                                        || old_session.as_ref() == accepted_session)
                            });
                    if already_delivered_startup {
                        return encode_success(id, ResponseResult::Ok {});
                    }
                    let binding = self.team_view_binding(terminal_id);
                    if binding.is_some() || lifecycle_refresh || startup_refresh {
                        let (team_view_revision, role_revision, scope_key) = binding
                            .map(|(revision, role_revision, scope_key)| {
                                (Some(revision), role_revision, scope_key)
                            })
                            .unwrap_or_else(|| {
                                let role_revision =
                                    self.state.terminals[terminal_id].overseer.revision;
                                (None, role_revision, "unassigned".to_string())
                            });
                        let (previous_offer, offer_state) = self
                            .state
                            .terminals
                            .get(terminal_id)
                            .map(|terminal| {
                                (terminal.orientation_offer.clone(), terminal.orientation)
                            })
                            .unwrap_or((
                                None,
                                crate::agent_orientation::OrientationState::Suppressed,
                            ));
                        let unchanged = previous_offer.as_ref().is_some_and(|offer| {
                            offer.session_id == session_id.clone().unwrap_or_default()
                                && offer.team_view_revision.as_deref()
                                    == team_view_revision.as_deref()
                                && offer.role_revision == role_revision
                                && offer.scope_key == scope_key
                        });
                        if unchanged
                            && offer_state == crate::agent_orientation::OrientationState::Offered
                        {
                            if let Some(offer) = previous_offer {
                                return encode_success(
                                    id,
                                    ResponseResult::OrientationContext {
                                        context: offer.context,
                                        nonce: offer.nonce,
                                    },
                                );
                            }
                        }
                        if !unchanged
                            || ((lifecycle_refresh || startup_refresh)
                                && offer_state
                                    != crate::agent_orientation::OrientationState::Offered)
                        {
                            let context = format!(
                                "{}{}\n[/Herdr orientation]",
                                crate::agent_orientation::OPEN,
                                self.orientation_context(terminal_id)
                            );
                            // Each new offer is its own delivery identity. A retry of a pending
                            // offer returns the stored one above, so only real refreshes get a new nonce.
                            let generation = self
                                .state
                                .terminals
                                .get_mut(terminal_id)
                                .map(|terminal| {
                                    terminal.orientation_generation += 1;
                                    terminal.orientation_generation
                                })
                                .unwrap_or(0);
                            let nonce = format!(
                                "{}:{}:{}:{}:{}",
                                terminal_id,
                                session_id.as_deref().unwrap_or(""),
                                role_revision,
                                team_view_revision.as_deref().unwrap_or("unassigned"),
                                generation
                            );
                            let offer = crate::agent_orientation::OrientationOffer {
                                session_id: session_id.clone().unwrap_or_default(),
                                nonce: nonce.clone(),
                                context: context.clone(),
                                team_view_revision,
                                role_revision,
                                scope_key,
                            };
                            if let Some(terminal) = self.state.terminals.get_mut(terminal_id) {
                                terminal.orientation =
                                    crate::agent_orientation::OrientationState::Offered;
                                terminal.orientation_offer = Some(offer);
                            }
                            self.state.mark_session_dirty();
                            self.schedule_session_save();
                            return encode_success(
                                id,
                                ResponseResult::OrientationContext { context, nonce },
                            );
                        }
                    }
                }
            }
        }
        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_pane_ack_orientation(
        &mut self,
        id: String,
        params: PaneAckOrientationParams,
    ) -> String {
        let Some(pane_id) = self.parse_reporting_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        if !matches!(params.source.as_str(), "herdr:codex" | "herdr:claude") {
            return encode_error(
                id,
                "invalid_orientation_ack",
                "unrecognized orientation source",
            );
        }
        let Some(terminal_id) = self.state.terminal_id_for_any_pane(pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let current_team_binding = self.team_view_binding(&terminal_id);
        let accepted = self
            .state
            .terminals
            .get(&terminal_id)
            .and_then(|terminal| {
                terminal.orientation_offer.as_ref().filter(|offer| {
                    let role_still_matches = if offer.team_view_revision.is_some() {
                        current_team_binding.as_ref().is_some_and(
                            |(revision, role_revision, scope)| {
                                terminal.overseer.revision == offer.role_revision
                                    && *role_revision == offer.role_revision
                                    && scope == &offer.scope_key
                                    && offer.team_view_revision.as_deref()
                                        == Some(revision.as_str())
                            },
                        )
                    } else {
                        true
                    };
                    offer.session_id == params.agent_session_id
                        && offer.nonce == params.nonce
                        && role_still_matches
                })
            })
            .is_some();
        if !accepted {
            return encode_error(
                id,
                "invalid_orientation_ack",
                "orientation offer does not match this conversation",
            );
        }
        if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
            terminal.orientation = crate::agent_orientation::OrientationState::Delivered;
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();
        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_pane_report_metadata(
        &mut self,
        id: String,
        params: PaneReportMetadataParams,
    ) -> String {
        let Some(pane_id) = self.parse_reporting_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let agent_label = match params.agent.as_deref() {
            Some(agent) => match normalize_reported_agent_label(agent) {
                Some(agent_label) => Some(agent_label),
                None => return invalid_agent(id),
            },
            None => None,
        };
        let Some(source) = normalize_optional_text(Some(params.source)) else {
            return encode_error(id, "invalid_metadata_request", "missing metadata source");
        };
        let context_set = params.context_used_percent.is_some() || params.context_unknown;
        if params.context_used_percent.is_none() && params.context_unknown {
            // A session-bound unknown reading revokes only that conversation's cached figure.
            if let Some(terminal_id) = self.state.terminal_id_for_any_pane(pane_id) {
                self.clear_context_report(&terminal_id, params.session_id.as_deref());
            }
        }
        if let Some(percent) = params.context_used_percent {
            // Bound to the conversation the reading came from: another or unknown
            // conversation leaves the meter unknown.
            if let Some(terminal_id) = self.state.terminal_id_for_any_pane(pane_id) {
                let label = if source == "herdr:claude-statusline" {
                    "Claude status line"
                } else {
                    source.as_str()
                };
                self.report_context_from_session(
                    &terminal_id,
                    percent,
                    params.session_id.as_deref(),
                    label,
                    std::time::Instant::now(),
                );
            }
        }
        let raw_title_set = params.title.is_some();
        let raw_display_agent_set = params.display_agent.is_some();
        let raw_custom_status_set = params.custom_status.is_some();
        let raw_state_labels_set = !params.state_labels.is_empty();
        let ttl = params.ttl_ms.map(std::time::Duration::from_millis);
        let title = normalize_presentation_text(params.title);
        let display_agent = normalize_presentation_text(params.display_agent);
        let custom_status = normalize_custom_status(params.custom_status);
        let applies_to_source = match params.applies_to_source {
            Some(applies_to_source) => {
                let Some(applies_to_source) = normalize_optional_text(Some(applies_to_source))
                else {
                    return encode_error(
                        id,
                        "invalid_metadata_request",
                        "missing metadata authority source",
                    );
                };
                Some(applies_to_source)
            }
            None => None,
        };
        let state_labels = match normalize_state_labels(params.state_labels) {
            Ok(labels) => labels,
            Err(status) => {
                return encode_error(
                    id,
                    "invalid_state_label",
                    format!("unknown state label: {status}"),
                );
            }
        };
        if raw_title_set && params.clear_title
            || raw_display_agent_set && params.clear_display_agent
            || raw_custom_status_set && params.clear_custom_status
            || raw_state_labels_set && params.clear_state_labels
        {
            return encode_error(
                id,
                "invalid_metadata_request",
                "cannot set and clear the same metadata field",
            );
        }
        if title.is_none()
            && display_agent.is_none()
            && custom_status.is_none()
            && state_labels.is_empty()
            && !params.clear_title
            && !params.clear_display_agent
            && !params.clear_custom_status
            && !params.clear_state_labels
        {
            if context_set {
                return encode_success(id, ResponseResult::Ok {});
            }
            return encode_error(
                id,
                "invalid_metadata_request",
                "missing metadata field to set or clear",
            );
        }
        self.handle_internal_event(crate::events::AppEvent::HookMetadataReported {
            pane_id,
            source,
            agent_label,
            applies_to_source,
            title,
            display_agent,
            custom_status,
            state_labels,
            clear_title: params.clear_title,
            clear_display_agent: params.clear_display_agent,
            clear_custom_status: params.clear_custom_status,
            clear_state_labels: params.clear_state_labels,
            seq: params.seq,
            ttl,
        });

        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_pane_clear_agent_authority(
        &mut self,
        id: String,
        params: PaneClearAgentAuthorityParams,
    ) -> String {
        let Some(pane_id) = self.parse_reporting_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        self.handle_internal_event(crate::events::AppEvent::HookAuthorityCleared {
            pane_id,
            source: params.source,
            seq: params.seq,
        });

        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_pane_release_agent(
        &mut self,
        id: String,
        params: PaneReleaseAgentParams,
    ) -> String {
        let Some(pane_id) = self.parse_reporting_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(agent_label) = normalize_reported_agent_label(&params.agent) else {
            return invalid_agent(id);
        };
        self.handle_internal_event(crate::events::AppEvent::HookAgentReleased {
            pane_id,
            source: params.source,
            known_agent: crate::detect::parse_agent_label(&agent_label),
            agent_label,
            seq: params.seq,
        });

        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_pane_send_text(
        &mut self,
        id: String,
        params: PaneSendTextParams,
    ) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(runtime) = self.lookup_runtime_sender(ws_idx, pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let origin = match self.message_origin_for_api(
            pane_id,
            params.origin.as_deref(),
            params.correlation_id.as_deref(),
            params.task_id.as_deref(),
        ) {
            Ok(origin) => origin,
            Err(message) => return encode_error(id, "invalid_message_origin", message),
        };
        if let Err(err) = runtime.try_send_bytes(Bytes::from(params.text.clone())) {
            return encode_error(id, "pane_send_failed", err.to_string());
        }
        if !params.text.trim().is_empty() {
            self.suppress_pending_orientation(pane_id);
            self.store_message_origin(pane_id, origin);
        }

        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_pane_send_input(
        &mut self,
        id: String,
        params: PaneSendInputParams,
    ) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let origin = match self.message_origin_for_api(
            pane_id,
            params.origin.as_deref(),
            params.correlation_id.as_deref(),
            params.task_id.as_deref(),
        ) {
            Ok(origin) => origin,
            Err(message) => return encode_error(id, "invalid_message_origin", message),
        };
        if params.keys.len() == 1 && params.keys[0].eq_ignore_ascii_case("enter") {
            return match self.send_agent_task(pane_id, &params.text) {
                Ok(()) => {
                    if !params.text.trim().is_empty() {
                        self.store_message_origin(pane_id, origin);
                    }
                    encode_success(id, ResponseResult::Ok {})
                }
                Err((code, message)) => encode_error(
                    id,
                    if code == "agent_send_failed" {
                        "pane_send_failed"
                    } else {
                        code
                    },
                    message,
                ),
            };
        }
        let Some(runtime) = self.lookup_runtime_sender(ws_idx, pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let encoded_keys = match encode_api_keys(runtime, &params.keys) {
            Ok(encoded_keys) => encoded_keys,
            Err(key) => return encode_error(id, "invalid_key", format!("unsupported key {key}")),
        };
        let trailing: Vec<u8> = encoded_keys.into_iter().flatten().collect();
        let submits_with_enter = params
            .keys
            .iter()
            .any(|key| key.eq_ignore_ascii_case("enter"));
        if submits_with_enter {
            let text = if params.text.is_empty() {
                Vec::new()
            } else {
                encode_api_text(runtime, &params.text)
            };
            if let Err((code, message)) = deliver_text_then_keys(runtime, &text, &trailing) {
                return encode_error(
                    id,
                    if code == "agent_send_failed" {
                        "pane_send_failed"
                    } else {
                        code
                    },
                    message,
                );
            }
        } else {
            let mut batch = if params.text.is_empty() {
                Vec::new()
            } else {
                encode_api_text(runtime, &params.text)
            };
            batch.extend(trailing);
            if !batch.is_empty() {
                if let Err(err) = runtime.try_send_bytes(Bytes::from(batch)) {
                    return encode_error(id, "pane_send_failed", err.to_string());
                }
            }
        }
        if !params.text.trim().is_empty() {
            self.suppress_pending_orientation(pane_id);
            self.store_message_origin(pane_id, origin);
        }

        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_pane_close(&mut self, id: String, target: PaneTarget) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&target.pane_id) else {
            return pane_not_found(id, &target.pane_id);
        };
        if self.state.close_pane_would_close_workspace(ws_idx, pane_id)
            && self.state.confirm_implicit_worktree_group_close(ws_idx)
        {
            return encode_error(
                id,
                "confirmation_required",
                "closing this pane would close a worktree group",
            );
        }
        let workspace_id = self.state.workspaces[ws_idx].id.clone();
        let terminal_id = self.state.terminal_id_for_pane(ws_idx, pane_id);
        // Closing a pane sets its agent down rather than ending it: the agent
        // keeps running and keeps its table row, with no pane showing it.
        self.state.set_down_agent_pane_by_id(ws_idx, pane_id);
        let should_close_workspace = {
            let Some(ws) = self.state.workspaces.get_mut(ws_idx) else {
                return pane_not_found(id, &target.pane_id);
            };
            ws.close_pane(pane_id)
        };
        if should_close_workspace {
            self.state.selected = ws_idx;
            self.state.close_selected_workspace();
            self.shutdown_detached_terminal_runtimes();
            self.emit_event(EventEnvelope {
                event: EventKind::PaneClosed,
                data: EventData::PaneClosed {
                    pane_id: target.pane_id.clone(),
                    workspace_id: workspace_id.clone(),
                },
            });
            self.emit_event(EventEnvelope {
                event: EventKind::WorkspaceClosed,
                data: EventData::WorkspaceClosed { workspace_id },
            });
        } else {
            self.state.remove_unattached_terminal_ids(terminal_id);
            self.shutdown_detached_terminal_runtimes();
            self.schedule_session_save();
            self.emit_event(EventEnvelope {
                event: EventKind::PaneClosed,
                data: EventData::PaneClosed {
                    pane_id: target.pane_id,
                    workspace_id,
                },
            });
        }

        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_pane_send_keys(
        &mut self,
        id: String,
        params: PaneSendKeysParams,
    ) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(runtime) = self.lookup_runtime_sender(ws_idx, pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let encoded_keys = match encode_api_keys(runtime, &params.keys) {
            Ok(encoded_keys) => encoded_keys,
            Err(key) => return encode_error(id, "invalid_key", format!("unsupported key {key}")),
        };
        let batch: Vec<u8> = encoded_keys.into_iter().flatten().collect();
        if !batch.is_empty() {
            if let Err(err) = runtime.try_send_bytes(Bytes::from(batch)) {
                return encode_error(id, "pane_send_failed", err.to_string());
            }
        }

        encode_success(id, ResponseResult::Ok {})
    }
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    let value = value?.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn normalize_presentation_text(value: Option<String>) -> Option<String> {
    let trimmed = value?.trim().to_string();
    let normalized: String = trimmed
        .chars()
        .filter(|ch| !ch.is_control())
        .take(80)
        .collect();
    (!normalized.trim().is_empty()).then(|| normalized.trim().to_string())
}

fn normalize_state_labels(
    labels: std::collections::HashMap<String, String>,
) -> Result<std::collections::HashMap<String, String>, String> {
    labels
        .into_iter()
        .map(|(status, label)| {
            let status = status.trim().to_ascii_lowercase();
            if !matches!(
                status.as_str(),
                "idle" | "working" | "blocked" | "done" | "unknown"
            ) {
                return Err(status);
            }
            Ok(normalize_presentation_text(Some(label)).map(|label| (status, label)))
        })
        .filter_map(Result::transpose)
        .collect()
}

fn pane_not_found(id: String, pane_id: &str) -> String {
    encode_error(id, "pane_not_found", format!("pane {pane_id} not found"))
}

fn invalid_agent(id: String) -> String {
    encode_error(id, "invalid_agent", "agent label must not be empty")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{api::schema::SuccessResponse, config::Config, workspace::Workspace};

    fn app_with_linked_worktree() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("issue")];
        app.state.workspaces[0].worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "herdr".into(),
            repo_root: "/repo/herdr".into(),
            checkout_path: "/repo/herdr-issue".into(),
            is_linked_worktree: true,
        });
        app
    }

    pub(super) fn app_with_two_panes() -> (App, crate::layout::PaneId) {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("test")];
        app.state.active = Some(0);
        let pane_id = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        app.state.ensure_test_terminals();
        let terminal_id = app.state.terminal_id_for_pane(0, pane_id).unwrap();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(
                Some(crate::detect::Agent::Pi),
                crate::detect::AgentState::Idle,
            );
        (app, pane_id)
    }

    #[tokio::test]
    async fn a_set_down_agent_records_the_session_it_reports_so_a_restart_can_resume_it() {
        let (mut app, _pane_id) = app_with_two_panes();

        let detached_pane_id = app
            .start_hidden_agent(
                std::env::current_dir().unwrap(),
                &["sleep".to_string(), "30".to_string()],
                Some(crate::detect::Agent::Claude),
            )
            .unwrap_or_else(|_| panic!("hidden agent should start"));

        let response = app.handle_pane_report_agent_session(
            "report".into(),
            PaneReportAgentSessionParams {
                turn_reporting: false,
                startup_source: None,
                pane_id: format!("p_{}", detached_pane_id.raw()),
                source: "herdr:claude".into(),
                agent: "claude".into(),
                seq: Some(1),
                agent_session_id: Some("45ec6637-9d30-4baf-a6fa-78968afab90c".into()),
                agent_session_path: None,
            },
        );

        assert!(
            !response.contains("pane_not_found"),
            "an agent set down is still reporting about itself: {response}"
        );
        let terminal_id = app.state.detached_agents[0]
            .pane
            .attached_terminal_id
            .clone();
        let session = app.state.terminals[&terminal_id]
            .persisted_agent_session
            .as_ref()
            .expect("the reported session is the one a restart resumes");
        assert_eq!(session.agent, "claude");
        assert_eq!(
            session.session_ref.value,
            "45ec6637-9d30-4baf-a6fa-78968afab90c"
        );

        app.state.close_detached_agent(detached_pane_id);
        app.shutdown_detached_terminal_runtimes();
    }

    #[test]
    fn api_pane_close_sets_its_agent_down() {
        let (mut app, pane_id) = app_with_two_panes();
        let terminal_id = app.state.terminal_id_for_pane(0, pane_id).unwrap();
        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();

        app.handle_pane_close(
            "req".into(),
            PaneTarget {
                pane_id: public_pane_id,
            },
        );

        assert!(app.state.workspaces[0].pane_state(pane_id).is_none());
        assert!(app.state.terminals.contains_key(&terminal_id));
        assert_eq!(app.state.detached_agents.len(), 1);
        assert_eq!(app.state.detached_agents[0].pane_id, pane_id);
    }

    #[test]
    fn api_pane_close_closes_linked_worktree_workspace_only() {
        let mut app = app_with_linked_worktree();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();

        let response = app.handle_pane_close(
            "req".into(),
            PaneTarget {
                pane_id: public_pane_id,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(success.id, "req");
        assert_eq!(app.state.request_remove_linked_worktree, None);
        assert!(app.state.workspaces.is_empty());
    }
}

#[cfg(test)]
#[path = "panes_orientation_tests.rs"]
mod orientation_tests;

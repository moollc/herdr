//! Only local UI handlers call the role switch. The API reports its result.
use crate::app::{App, AppState};
use crate::detect::AgentState;
use crate::layout::PaneId;
use crate::overseer::{
    Holder, Kind, MessageOrigin, MessageOriginKind, PendingEvent, Scope, MAIL_PREFIX,
};
use crate::terminal::TerminalId;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

type Refusal = (&'static str, String);

/// An event found in one pass, with everyone who should hear it.
struct Queued {
    receivers: Vec<TerminalId>,
    key: String,
    plain: String,
    with_space: String,
    state: AgentState,
    reason: Option<String>,
    marker: Option<std::path::PathBuf>,
}

impl AppState {
    /// Each terminal that sits in a space, with the space's index. Terminals
    /// set down belong to no space and are absent.
    pub(crate) fn terminal_places(&self) -> HashMap<TerminalId, usize> {
        let mut places = HashMap::new();
        for (index, workspace) in self.workspaces.iter().enumerate() {
            for tab in &workspace.tabs {
                for pane in tab.panes.values() {
                    places.insert(pane.attached_terminal_id.clone(), index);
                }
            }
        }
        places
    }

    /// The space's own setting wins over the `[overseer] scope` setting.
    pub(crate) fn effective_overseer_scope(&self, space: Option<usize>) -> Scope {
        space
            .and_then(|index| self.workspaces.get(index))
            .and_then(|workspace| workspace.overseer_scope)
            .unwrap_or(self.overseer_scope)
    }

    /// One control, one step round: with a session scope a pane goes from
    /// none to the one overseer (the chief) and back; with a workspace scope
    /// it goes none, overseer of its space, chief, none. A chief is one per
    /// session and a space has one overseer; taking a role releases the
    /// pane that held it, except that stepping a space overseer on to chief
    /// is skipped while another pane is the chief.
    pub(crate) fn toggle_oversee(&mut self, pane: PaneId) {
        let Some(id) = self.terminal_id_for_any_pane(pane) else {
            return;
        };
        let places = self.terminal_places();
        let space = places.get(&id).copied();
        let current = self.terminals.get(&id).and_then(|t| t.overseer.kind());
        let next = match (self.effective_overseer_scope(space), current) {
            (Scope::Session, None) => Some(Kind::Chief),
            (Scope::Workspace, None) => Some(if space.is_some() {
                Kind::Space
            } else {
                Kind::Chief
            }),
            (Scope::Workspace, Some(Kind::Space)) => {
                let other_chief = self
                    .terminals
                    .iter()
                    .any(|(other, t)| *other != id && t.overseer.kind() == Some(Kind::Chief));
                (!other_chief).then_some(Kind::Chief)
            }
            _ => None,
        };
        for (other, terminal) in self.terminals.iter_mut() {
            let wanted = if *other == id {
                next
            } else {
                match (terminal.overseer.kind(), next) {
                    (Some(Kind::Chief), Some(Kind::Chief)) => None,
                    (Some(Kind::Space), Some(Kind::Space))
                        if places.get(other).copied() == space =>
                    {
                        None
                    }
                    (held, _) => held,
                }
            };
            if terminal.overseer.kind() != wanted {
                terminal.overseer.assign(wanted);
                terminal.revision = terminal.revision.wrapping_add(1);
            }
        }
        self.mark_session_dirty();
    }

    pub(crate) fn toggle_space_overseer(&mut self, pane: PaneId) {
        let Some(id) = self.terminal_id_for_any_pane(pane) else {
            return;
        };
        let enabled = self
            .terminals
            .get(&id)
            .is_some_and(|terminal| !terminal.overseer.is_space_overseer());
        self.set_space_overseer(&id, enabled);
    }

    pub(crate) fn toggle_overlord(&mut self, pane: PaneId) {
        let Some(id) = self.terminal_id_for_any_pane(pane) else {
            return;
        };
        let enabled = self
            .terminals
            .get(&id)
            .is_some_and(|terminal| !terminal.overseer.is_session_overlord());
        self.set_session_overlord(&id, enabled);
    }

    pub(crate) fn set_space_overseer(&mut self, id: &TerminalId, enabled: bool) -> bool {
        let places = self.terminal_places();
        let Some(space) = places.get(id).copied() else {
            return false;
        };
        let mut previous_holder = None;
        if enabled {
            let previous: Vec<_> = self
                .terminals
                .iter()
                .filter(|(other, terminal)| {
                    *other != id
                        && places.get(*other) == Some(&space)
                        && terminal.overseer.is_space_overseer()
                })
                .map(|(other, _)| other.clone())
                .collect();
            previous_holder = previous.first().cloned();
            for previous_id in previous {
                if self.change_role(&previous_id, Kind::Space, false) {
                    self.annotate_role_transition(
                        &previous_id,
                        Kind::Space,
                        Some(space),
                        Some(&previous_id),
                        Some(id),
                    );
                }
            }
        }
        let changed = self.change_role(id, Kind::Space, enabled);
        if changed {
            self.annotate_role_transition(
                id,
                Kind::Space,
                Some(space),
                if enabled {
                    previous_holder.as_ref()
                } else {
                    Some(id)
                },
                enabled.then_some(id),
            );
        }
        changed
    }

    pub(crate) fn set_session_overlord(&mut self, id: &TerminalId, enabled: bool) -> bool {
        let mut previous_holder = None;
        if enabled {
            let previous: Vec<_> = self
                .terminals
                .iter()
                .filter(|(other, terminal)| *other != id && terminal.overseer.is_session_overlord())
                .map(|(other, _)| other.clone())
                .collect();
            previous_holder = previous.first().cloned();
            for previous_id in previous {
                if self.change_role(&previous_id, Kind::Chief, false) {
                    self.annotate_role_transition(
                        &previous_id,
                        Kind::Chief,
                        None,
                        Some(&previous_id),
                        Some(id),
                    );
                }
            }
        }
        let changed = self.change_role(id, Kind::Chief, enabled);
        if changed {
            self.annotate_role_transition(
                id,
                Kind::Chief,
                None,
                if enabled {
                    previous_holder.as_ref()
                } else {
                    Some(id)
                },
                enabled.then_some(id),
            );
        }
        changed
    }

    pub(crate) fn reconcile_moved_space_role(&mut self, id: &TerminalId) {
        let places = self.terminal_places();
        let Some(space) = places.get(id).copied() else {
            return;
        };
        let moving_has_role = self
            .terminals
            .get(id)
            .is_some_and(|terminal| terminal.overseer.is_space_overseer());
        let destination_has_other = self.terminals.iter().any(|(other, terminal)| {
            other != id
                && places.get(other) == Some(&space)
                && terminal.overseer.is_space_overseer()
        });
        if moving_has_role && destination_has_other && self.change_role(id, Kind::Space, false) {
            let replacement = self.terminals.iter().find_map(|(other, terminal)| {
                (other != id
                    && places.get(other) == Some(&space)
                    && terminal.overseer.is_space_overseer())
                .then(|| other.clone())
            });
            self.annotate_role_transition(
                id,
                Kind::Space,
                Some(space),
                Some(id),
                replacement.as_ref(),
            );
        }
    }

    fn annotate_role_transition(
        &mut self,
        id: &TerminalId,
        role: Kind,
        space: Option<usize>,
        old_holder: Option<&TerminalId>,
        replacement: Option<&TerminalId>,
    ) {
        let scope = match role {
            Kind::Chief => "session".to_string(),
            Kind::Space => space
                .and_then(|index| self.workspaces.get(index))
                .map(|workspace| format!("space:{}", workspace.id))
                .unwrap_or_else(|| "space:unknown".into()),
        };
        let detail = format!(
            "role={} revision={} scope={} old_holder={} replacement_holder={}",
            role.as_str(),
            self.terminals
                .get(id)
                .map_or(0, |terminal| terminal.overseer.revision),
            scope,
            old_holder.map_or_else(|| "none".to_string(), ToString::to_string),
            replacement.map_or_else(|| "none".to_string(), ToString::to_string),
        );
        let revision = self
            .terminals
            .get(id)
            .map_or(0, |terminal| terminal.overseer.revision);
        if let Some(notice) = self
            .terminals
            .get_mut(id)
            .and_then(|terminal| terminal.overseer.notices.back_mut())
        {
            if notice.role == Some(role) && notice.role_revision == revision {
                notice.detail = detail;
            }
        }
    }

    fn change_role(&mut self, id: &TerminalId, kind: Kind, enabled: bool) -> bool {
        let Some(terminal) = self.terminals.get_mut(id) else {
            return false;
        };
        if !terminal.overseer.set_membership(kind, enabled) {
            return false;
        }
        terminal.revision = terminal.revision.wrapping_add(1);
        self.mark_session_dirty();
        true
    }

    /// Step a space through "follow the setting", "its own overseer" and
    /// "one overseer for the session". Returns the new choice for the toast.
    pub(crate) fn cycle_space_overseer_scope(&mut self, space: usize) -> Option<Option<Scope>> {
        let workspace = self.workspaces.get_mut(space)?;
        workspace.overseer_scope = match workspace.overseer_scope {
            None => Some(Scope::Workspace),
            Some(Scope::Workspace) => Some(Scope::Session),
            Some(Scope::Session) => None,
        };
        let choice = workspace.overseer_scope;
        self.mark_session_dirty();
        Some(choice)
    }
}

impl App {
    pub(super) fn overseer_team_view(&self, holder_id: &TerminalId) -> String {
        let Some(holder) = self.state.terminals.get(holder_id) else {
            return "unknown holder".into();
        };
        let is_overlord = holder.overseer.is_session_overlord();
        let own_space = self.state.terminal_places().get(holder_id).copied();
        let overlord = self
            .state
            .terminals
            .iter()
            .find(|(_, terminal)| terminal.overseer.is_session_overlord())
            .map(|(id, _)| {
                crate::pane_names::effective_name(&self.state.terminals, id)
                    .unwrap_or_else(|| id.to_string())
            })
            .unwrap_or_else(|| "unknown (looked at live role holders; none assigned)".into());
        let mut lines = Vec::new();
        for (workspace_index, workspace) in self.state.workspaces.iter().enumerate() {
            if !is_overlord && own_space != Some(workspace_index) {
                continue;
            }
            for tab in &workspace.tabs {
                for (pane, state) in &tab.panes {
                    if let Some(line) = self.overseer_team_pane(&state.attached_terminal_id, *pane)
                    {
                        lines.push(format!(
                            "{}: {line}",
                            one_line(
                                &workspace.display_name_from(
                                    &self.state.terminals,
                                    &self.terminal_runtimes
                                )
                            )
                        ));
                    }
                }
            }
        }
        if is_overlord {
            for detached in &self.state.detached_agents {
                if let Some(line) =
                    self.overseer_team_pane(&detached.pane.attached_terminal_id, detached.pane_id)
                {
                    lines.push(format!("detached: {line}"));
                }
            }
        }
        if !is_overlord {
            lines.push(format!("session overlord: {}", one_line(&overlord)));
        }
        if lines.is_empty() {
            "no panes in scope (looked at live workspace layout)".into()
        } else {
            lines.join("\n")
        }
    }

    fn overseer_team_pane(&self, id: &TerminalId, _pane: PaneId) -> Option<String> {
        let terminal = self.state.terminals.get(id)?;
        let name = crate::pane_names::effective_name(&self.state.terminals, id)
            .unwrap_or_else(|| id.to_string());
        let agent = terminal.effective_agent_label().unwrap_or("unknown");
        let observed = terminal.model_info.as_ref();
        let observed_at = self.agent_model_cache.get(id).map(|entry| {
            let at = entry
                .modified
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            format!("source={} observed_at={at}", entry.session_file.display())
        });
        let model = observed
            .map(|info| {
                format!(
                    "{} ({})",
                    one_line(&info.model),
                    observed_at.clone().unwrap_or_else(|| {
                        "observed session metadata; timestamp unavailable".into()
                    })
                )
            })
            .unwrap_or_else(|| {
                if terminal.effective_known_agent() == Some(crate::detect::Agent::Claude)
                    && self.agent_model_cache.get(id).is_some_and(|entry| entry.info.is_none())
                {
                    let source = self.agent_model_cache.get(id)
                        .map(|entry| entry.session_file.display().to_string())
                        .unwrap_or_else(|| "Claude session transcript".into());
                    return format!("no assistant turn yet (looked at {source})");
                }
                if terminal.effective_known_agent() == Some(crate::detect::Agent::Cursor) {
                    return "unknown (Cursor does not expose a transcript model field to Herdr; looked for a native model observer, none is available)".into();
                }
                let source = self
                    .agent_model_cache
                    .get(id)
                    .map(|entry| entry.session_file.display().to_string())
                    .or_else(|| {
                        terminal.model_probe_session().map(|(_, session)| {
                            format!("native session {session}; transcript lookup")
                        })
                    })
                    .unwrap_or_else(|| "hook/persisted native session binding".into());
                format!("unknown (model lookup found no observation; looked at {source})")
            });
        let effort = observed.and_then(|info| info.effort.as_deref()).map(one_line)
            .unwrap_or_else(|| "unknown (effort not present in observed session data; looked at current native session record)".into());
        let configured = terminal
            .model_pin
            .as_ref()
            .map(|pin| one_line(&pin.model))
            .unwrap_or_else(|| "none".into());
        let pane_name = name.clone();
        let queue_row = std::fs::read_to_string(&self.overseer_service.queue_path)
            .ok()
            .and_then(|contents| {
                contents
                    .lines()
                    .filter(|line| !line.starts_with('#'))
                    .filter_map(|line| {
                        let fields: Vec<_> = line.split('\t').collect();
                        (fields.len() > 13 && fields[1] == pane_name)
                            .then(|| fields.into_iter().map(str::to_string).collect::<Vec<_>>())
                    })
                    .next_back()
            });
        let stage = queue_row
            .map(|fields| {
                format!(
                    "{} title={} state={} out={} done_at={}",
                    fields[0],
                    one_line(&fields[10]),
                    fields[8],
                    fields[4],
                    fields[13]
                )
            })
            .unwrap_or_else(|| {
                format!(
                    "unknown (looked for pane {pane_name} in {} and found no stage row)",
                    self.overseer_service.queue_path.display()
                )
            });
        let marker =
            crate::overseer::queued_stage_marker(&self.overseer_service.queue_path, &pane_name);
        let stage = marker
            .map(|marker| {
                format!(
                    "{stage}; marker={}{}",
                    marker.display(),
                    std::fs::read_to_string(&marker)
                        .ok()
                        .and_then(|text| text
                            .split_once("HANDOFF")
                            .map(|(_, body)| one_line(body.trim())))
                        .map(|body| format!(" output={body}"))
                        .unwrap_or_default()
                )
            })
            .unwrap_or(stage);
        let provider = terminal.effective_known_agent().and_then(|agent| {
            crate::ui::usage_bar::provider_for_terminal(&self.state, Some(id), agent)
        });
        let allowance = provider
            .zip(self.state.usage_snapshot.as_ref())
            .and_then(|(provider, snapshot)| {
                snapshot
                    .providers
                    .iter()
                    .find(|usage| usage.provider == provider)
                    .map(|usage| {
                        usage
                            .windows
                            .iter()
                            .map(|window| {
                                format!(
                                    "{}% reset={}",
                                    window
                                        .left_pct
                                        .map_or_else(|| "unknown".into(), |pct| pct.to_string()),
                                    window.resets_at.map_or_else(
                                        || "unknown".into(),
                                        |reset| reset.to_string()
                                    )
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                            + &format!(
                                " source={} seen_at={}",
                                usage.source,
                                usage.seen_at.unwrap_or(0)
                            )
                    })
            })
            .unwrap_or_else(|| {
                format!(
                    "unknown (looked in current usage snapshot for provider {}; no reading found)",
                    provider.map_or("unknown", crate::ui::usage_bar::provider_name)
                )
            });
        let blocked = blocked_reason(terminal).unwrap_or_else(|| "none".into());
        let routing = routing_block_status(terminal, id);
        let bench = team_bench_status(&name);
        let finishes = team_stage_finishes(&self.overseer_service.queue_path, &name);
        let stage_state = match terminal.state {
            AgentState::Working => "working",
            AgentState::Idle => "idle",
            AgentState::Blocked => "blocked",
            AgentState::Unknown => "unknown",
        };
        Some(format!("{} harness={} model={} effort={} configured_pin={} state={} queue_stage={} allowance={} block={} routing_block={} bench={} stage_finishes={}",
            one_line(&name), agent, model, effort, configured, stage_state, stage, allowance, one_line(&blocked), routing, bench, finishes))
    }

    pub(crate) fn overseer_deadline(&self) -> Option<Instant> {
        self.state
            .terminals
            .values()
            .any(|t| t.overseer.active || !t.overseer.notices.is_empty())
            .then_some(self.overseer_service.next_tick)
    }

    pub(crate) fn advance_overseer(&mut self, now: Instant) -> bool {
        if now < self.overseer_service.next_tick {
            return false;
        }
        self.overseer_service.next_tick = now + Duration::from_secs(1);
        self.collect_overseer_events(now);
        self.advance_context_transitions(now);
        let holders: Vec<(TerminalId, Kind)> = self
            .overseer_service
            .holders
            .iter()
            .map(|(id, holder)| (id.clone(), holder.kind))
            .collect();
        for (id, kind) in &holders {
            let due = self.overseer_service.holders[id]
                .written_at
                .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(10));
            if due {
                if let Err(error) = self.write_situation(id, *kind) {
                    tracing::warn!(%error, "cannot write overseer situation");
                }
                let path = self.overseer_situation_path(id, *kind);
                if let Some(holder) = self.overseer_service.holders.get_mut(id) {
                    holder.written_at = Some(now);
                    holder.file = Some(path);
                }
            }
        }
        let pending: Vec<_> = self
            .state
            .terminals
            .iter()
            .filter(|(_, t)| !t.overseer.notices.is_empty())
            .map(|(id, _)| id.clone())
            .collect();
        let mut changed = false;
        for id in pending {
            let notice = loop {
                let Some(notice) = self.state.terminals[&id].overseer.notices.front().cloned()
                else {
                    break None;
                };
                let role = notice
                    .role
                    .or_else(|| self.state.terminals[&id].overseer.kind());
                let current_revision = self.state.terminals[&id].overseer.revision;
                let still_holds_role = role.is_some_and(|role| match role {
                    Kind::Space => self.state.terminals[&id].overseer.is_space_overseer(),
                    Kind::Chief => self.state.terminals[&id].overseer.is_session_overlord(),
                });
                if (notice.role_revision != 0 && notice.role_revision != current_revision)
                    || (notice.active && !still_holds_role)
                    || (role.is_none() && notice.active)
                {
                    if let Some(terminal) = self.state.terminals.get_mut(&id) {
                        terminal.overseer.notices.pop_front();
                    }
                    changed = true;
                    continue;
                }
                break Some(notice);
            };
            let Some(notice) = notice else { continue };
            let role = notice
                .role
                .or_else(|| self.state.terminals[&id].overseer.kind());
            if !self.overseer_ready(&id, now) {
                self.overseer_service.profile_probes.remove(&id);
                continue;
            }
            let mut notice = notice;
            if !notice.profile_checked {
                let role_service_active = self.state.terminals[&id].overseer.active;
                let result = self.overseer_profile(&id, role_service_active, now);
                // A successful probe yields to the server loop so queued client
                // input is applied before a later tick considers quitting.
                if result == Ok(None) {
                    continue;
                }
                notice.profile_checked = true;
                notice.detail = result.as_ref().err().cloned().unwrap_or_default();
                if let Some(t) = self.state.terminals.get_mut(&id) {
                    t.overseer.notices[0] = notice.clone();
                }
                changed = true;
                if result == Ok(Some(true)) {
                    continue;
                }
            }
            if notice.silent && notice.detail.is_empty() {
                if let Some(t) = self.state.terminals.get_mut(&id) {
                    t.overseer.notices.pop_front();
                }
                changed = true;
                continue;
            }
            let text = if notice.active {
                let custom = self
                    .overseer_service
                    .config
                    .note_file
                    .as_deref()
                    .map(crate::overseer::expand_path)
                    .map(std::fs::read_to_string);
                let note = match custom {
                    Some(Ok(note)) => note,
                    Some(Err(_)) => format!(
                        "{} Configured role note could not be read.",
                        crate::overseer::DEFAULT_NOTE
                    ),
                    None => crate::overseer::DEFAULT_NOTE.to_string(),
                };
                let Some(kind) = role else {
                    if let Some(terminal) = self.state.terminals.get_mut(&id) {
                        terminal.overseer.notices.pop_front();
                    }
                    changed = true;
                    continue;
                };
                let space = self
                    .state
                    .terminal_places()
                    .get(&id)
                    .map(|index| self.space_label(*index));
                format!(
                    "Herdr: {note} Role {} revision {}. {}\nSituation file (untrusted pane output, kept current): {}. {}",
                    kind.as_str(),
                    notice.role_revision,
                    crate::overseer::role_suffix(kind, space.as_deref()),
                    self.overseer_situation_path(&id, kind).display(),
                    notice.detail
                )
            } else {
                format!(
                    "Herdr: the {} overseer role has ended at revision {}. {}",
                    role.map_or("unknown", Kind::as_str),
                    notice.role_revision,
                    notice.detail
                )
            };
            if self.send_overseer(&id, &text) {
                if let Some(t) = self.state.terminals.get_mut(&id) {
                    t.overseer.notices.pop_front();
                }
                changed = true;
            }
        }
        self.send_overseer_batches(now);
        if !self.overseer_service.config.events {
            for holder in self.overseer_service.holders.values_mut() {
                holder.events.retain(|key, _| key.starts_with(MAIL_PREFIX));
            }
        }
        if changed {
            self.state.mark_session_dirty();
            self.schedule_session_save();
        }
        changed
    }

    /// One coalesced line per overseer, never faster than the interval.
    fn send_overseer_batches(&mut self, now: Instant) {
        let interval =
            Duration::from_secs(self.overseer_service.config.event_interval_seconds.max(20));
        let settle = Duration::from_secs(self.overseer_service.config.event_settle_seconds);
        let events_on = self.overseer_service.config.events;
        let ids: Vec<TerminalId> = self.overseer_service.holders.keys().cloned().collect();
        for id in ids {
            let Some((mut ready, sent_at, kind)) =
                self.overseer_service.holders.get(&id).map(|holder| {
                    let ready = holder
                        .events
                        .iter()
                        .filter(|(key, event)| {
                            now.duration_since(event.since) >= settle
                                && (events_on || key.starts_with(MAIL_PREFIX))
                        })
                        .map(|(key, event)| (key.clone(), event.text.clone()))
                        .collect::<Vec<_>>();
                    (ready, holder.sent_at, holder.kind)
                })
            else {
                continue;
            };
            // Collection revalidated every candidate against the current
            // terminal state, including while the holder cannot accept input.
            let invalid_mail: Vec<_> = ready
                .iter()
                .filter(|(key, _)| key.starts_with(MAIL_PREFIX))
                .filter_map(|(key, _)| {
                    self.mail_invalidation(key)
                        .map(|reason| (key.clone(), reason))
                })
                .collect();
            for (key, reason) in invalid_mail {
                ready.retain(|(candidate, _)| candidate != &key);
                if let Some(holder) = self.overseer_service.holders.get_mut(&id) {
                    holder.events.remove(&key);
                }
                if let Some(delivery) = self.overseer_service.message_deliveries.get_mut(&key) {
                    delivery.state = crate::overseer::MessageDeliveryState::Cancelled;
                    delivery.hold_reason = Some(reason);
                }
            }
            if ready.is_empty()
                || !sent_at.is_none_or(|last| now.duration_since(last) >= interval)
                || !self.state.terminals[&id].overseer.notices.is_empty()
                || !self.overseer_ready(&id, now)
            {
                let reason = self.mail_hold_reason(&id, now);
                if let Some(holder) = self.overseer_service.holders.get(&id) {
                    for key in holder
                        .events
                        .keys()
                        .filter(|key| key.starts_with(MAIL_PREFIX))
                    {
                        if let Some(delivery) =
                            self.overseer_service.message_deliveries.get_mut(key)
                        {
                            delivery.hold_reason = reason.clone();
                        }
                    }
                }
                continue;
            }
            let text = format!(
                "Herdr: {}. Situation: {}",
                ready
                    .iter()
                    .map(|(_, text)| text.as_str())
                    .collect::<Vec<_>>()
                    .join("; "),
                self.overseer_situation_path(&id, kind).display()
            );
            if self.send_overseer(&id, &text) {
                let submitted_mail = ready
                    .iter()
                    .filter(|(key, _)| key.starts_with(MAIL_PREFIX))
                    .map(|(key, _)| key.clone())
                    .collect::<Vec<_>>();
                if let Some(holder) = self.overseer_service.holders.get_mut(&id) {
                    for (key, _) in &ready {
                        holder.events.remove(key);
                    }
                    holder.sent_at = Some(now);
                }
                for key in submitted_mail {
                    if let Some(delivery) = self.overseer_service.message_deliveries.get_mut(&key) {
                        delivery.state = crate::overseer::MessageDeliveryState::Submitted;
                        delivery.hold_reason = None;
                    }
                }
            }
        }
    }

    fn mail_invalidation(&self, message_id: &str) -> Option<String> {
        let delivery = self.overseer_service.message_deliveries.get(message_id)?;
        let address_valid = match delivery.direction {
            crate::overseer::MessageDirection::Escalation => {
                delivery.sender_role == Kind::Space && delivery.receiver_role == Kind::Chief
            }
            crate::overseer::MessageDirection::Downward => {
                delivery.sender_role == Kind::Chief && delivery.receiver_role == Kind::Space
            }
        };
        if !address_valid {
            return Some("message direction and addressed roles do not agree".into());
        }
        let Some(sender) = self.state.terminals.get(&delivery.sender_terminal_id) else {
            return Some("sender terminal no longer exists".into());
        };
        let sender_holds = match delivery.sender_role {
            Kind::Space => sender.overseer.is_space_overseer(),
            Kind::Chief => sender.overseer.is_session_overlord(),
        };
        if sender.overseer.generation(delivery.sender_role) != delivery.sender_role_generation {
            return Some("sender role generation changed after the message was queued".into());
        }
        if !sender_holds {
            return Some("sender no longer holds the sending role".into());
        }
        if sender.current_session_identity_for_persistence() != delivery.sender_session
            || delivery.sender_session.is_none()
        {
            return Some("sender native conversation changed or was not observable".into());
        }
        if delivery.sender_role == Kind::Space {
            let workspace = self
                .state
                .terminal_places()
                .get(&delivery.sender_terminal_id)
                .and_then(|index| self.state.workspaces.get(*index))
                .map(|workspace| workspace.id.clone());
            if workspace != delivery.sender_workspace_id {
                return Some("sender moved out of the authorized space".into());
            }
        }
        let Some(receiver) = self.state.terminals.get(&delivery.target_terminal_id) else {
            return Some("receiver terminal no longer exists".into());
        };
        let receiver_holds = match delivery.receiver_role {
            Kind::Space => receiver.overseer.is_space_overseer(),
            Kind::Chief => receiver.overseer.is_session_overlord(),
        };
        if receiver.overseer.generation(delivery.receiver_role) != delivery.receiver_role_generation
        {
            return Some("receiver role generation changed after the message was queued".into());
        }
        if !receiver_holds {
            return Some("receiver no longer holds the addressed role".into());
        }
        if receiver.current_session_identity_for_persistence() != delivery.receiver_session
            || delivery.receiver_session.is_none()
        {
            return Some("receiver native conversation changed or was not observable".into());
        }
        None
    }

    fn mail_hold_reason(&self, id: &TerminalId, now: Instant) -> Option<String> {
        if self
            .state
            .terminals
            .get(id)
            .is_some_and(|terminal| !terminal.overseer.notices.is_empty())
        {
            return Some("pending role notice".into());
        }
        let Some(terminal) = self.state.terminals.get(id) else {
            return Some("receiver terminal is unknown".into());
        };
        if terminal.state != AgentState::Idle {
            return Some("receiver is busy or blocked".into());
        }
        if terminal.effective_known_agent().is_none() {
            return Some("receiver harness is unknown".into());
        }
        let Some(runtime) = self.terminal_runtimes.get(id) else {
            return Some("receiver input is unknown; no live terminal runtime".into());
        };
        if !runtime.input_is_quiet(now) {
            return Some("receiver input is busy".into());
        }
        if !self.input_box_empty(id, now) {
            return Some("receiver has a draft in the input box".into());
        }
        if self
            .overseer_service
            .holders
            .get(id)
            .is_some_and(|holder| holder.mail_waiting() >= crate::overseer::MAIL_LIMIT)
        {
            Some("mailbox capacity reached".into())
        } else {
            Some("delivery interval has not elapsed".into())
        }
    }

    /// The mail delivery guard for the input box: no recent typing and an
    /// empty prompt. Mail, allowance lines and the context transition share it.
    pub(super) fn input_box_empty(&self, id: &TerminalId, now: Instant) -> bool {
        self.terminal_runtimes.get(id).is_some_and(|rt| {
            rt.input_is_quiet(now)
                && crate::overseer::empty_prompt_ansi(&rt.detection_text(), &rt.recent_ansi(80))
        })
    }

    pub(super) fn overseer_ready(&self, id: &TerminalId, now: Instant) -> bool {
        self.state.terminals.get(id).is_some_and(|t| {
            t.state == AgentState::Idle
                && t.effective_known_agent().is_some()
                && t.pending_restart.is_none()
        }) && self.input_box_empty(id, now)
    }

    pub(super) fn clear_pending_notification_origin(&mut self, pane: PaneId) {
        let Some(terminal_id) = self.state.terminal_id_for_any_pane(pane) else {
            return;
        };
        let clear = self
            .state
            .terminals
            .get(&terminal_id)
            .is_some_and(|terminal| {
                terminal
                    .message_origin
                    .as_ref()
                    .is_some_and(|origin| origin.kind == MessageOriginKind::Notification)
            });
        if clear {
            if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                terminal.message_origin = None;
            }
            self.state.mark_session_dirty();
            self.schedule_session_save();
        }
    }

    pub(super) fn send_overseer(&mut self, id: &TerminalId, text: &str) -> bool {
        let sent = self
            .terminal_runtimes
            .get(id)
            .is_some_and(|rt| super::api_helpers::send_prompt(rt, text).is_ok());
        if sent {
            self.note_internal_notification(id);
        }
        sent
    }

    pub(super) fn message_origin_for_api(
        &self,
        pane: PaneId,
        origin: Option<&str>,
        correlation_id: Option<&str>,
        task_id: Option<&str>,
    ) -> Result<Option<MessageOrigin>, String> {
        let Some(origin) = origin else {
            return Ok(None);
        };
        let kind = match origin {
            "notification" => MessageOriginKind::Notification,
            "task" => MessageOriginKind::Task,
            _ => return Err("origin must be notification or task".into()),
        };
        let correlation_id = correlation_id
            .filter(|value| !value.trim().is_empty() && value.len() <= 256)
            .ok_or_else(|| "a bounded correlation_id is required for routed sends".to_string())?;
        let task_id = task_id.filter(|value| !value.trim().is_empty() && value.len() <= 256);
        if kind == MessageOriginKind::Task && task_id.is_none() {
            return Err("task origin requires a bounded task_id".into());
        }
        let terminal_id = self
            .state
            .terminal_id_for_any_pane(pane)
            .ok_or_else(|| "routed send target has no native terminal identity".to_string())?;
        let session = self.state.terminals[&terminal_id]
            .current_session_identity_for_persistence()
            .ok_or_else(|| "routed send target has no verified native agent session".to_string())?;
        Ok(Some(MessageOrigin {
            kind,
            correlation_id: correlation_id.to_string(),
            task_id: task_id.map(str::to_string),
            session,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            submitted_while_working: self.state.terminals[&terminal_id].state
                == AgentState::Working,
            submitted_turn_ends: self.state.terminals[&terminal_id]
                .turn_reporting
                .as_ref()
                .map_or(0, |report| report.ends),
            active: false,
        }))
    }

    pub(super) fn store_message_origin(&mut self, pane: PaneId, origin: Option<MessageOrigin>) {
        let Some(terminal_id) = self.state.terminal_id_for_any_pane(pane) else {
            return;
        };
        if let Some(origin) = origin {
            if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                terminal.message_origin = Some(origin);
            }
        } else if self
            .state
            .terminals
            .get(&terminal_id)
            .and_then(|terminal| terminal.message_origin.as_ref())
            .is_none_or(|origin| origin.kind == MessageOriginKind::Notification)
        {
            if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                terminal.message_origin = None;
            }
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();
    }

    fn note_internal_notification(&mut self, id: &TerminalId) {
        let Some(session) = self
            .state
            .terminals
            .get(id)
            .and_then(|terminal| terminal.current_session_identity_for_persistence())
        else {
            return;
        };
        let mut service = self.overseer_service.message_origin_seq;
        service = service.wrapping_add(1);
        self.overseer_service.message_origin_seq = service;
        if let Some(terminal) = self.state.terminals.get_mut(id) {
            terminal.message_origin = Some(MessageOrigin {
                kind: MessageOriginKind::Notification,
                correlation_id: format!("native-overseer-{id}-{service}"),
                task_id: None,
                session,
                created_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                submitted_while_working: terminal.state == AgentState::Working,
                submitted_turn_ends: terminal
                    .turn_reporting
                    .as_ref()
                    .map_or(0, |report| report.ends),
                active: false,
            });
            self.state.mark_session_dirty();
            self.schedule_session_save();
        }
    }

    fn space_label(&self, index: usize) -> String {
        self.state
            .workspaces
            .get(index)
            .map(|workspace| {
                one_line(
                    &workspace.display_name_from(&self.state.terminals, &self.terminal_runtimes),
                )
            })
            .unwrap_or_default()
    }

    /// Hold one entry per role holder, dropping entries whose last role ended.
    /// A terminal that holds both roles uses its session scope for shared
    /// history and receives each matching message only once.
    fn sync_overseer_holders(&mut self, now: Instant) {
        let wanted: HashMap<TerminalId, Kind> = self
            .state
            .terminals
            .iter()
            .filter_map(|(id, terminal)| {
                if terminal.overseer.is_session_overlord() {
                    Some((id.clone(), Kind::Chief))
                } else if terminal.overseer.is_space_overseer() {
                    Some((id.clone(), Kind::Space))
                } else {
                    None
                }
            })
            .collect();
        let service = &mut self.overseer_service;
        let was_empty = service.holders.is_empty();
        let released: Vec<_> = service
            .holders
            .keys()
            .filter(|id| !wanted.contains_key(*id))
            .cloned()
            .collect();
        for id in released {
            if let Some(holder) = service.holders.get(&id) {
                for key in holder
                    .events
                    .keys()
                    .filter(|key| key.starts_with(MAIL_PREFIX))
                {
                    if let Some(delivery) = service.message_deliveries.get_mut(key) {
                        delivery.state = crate::overseer::MessageDeliveryState::Cancelled;
                        delivery.hold_reason = Some("receiver role was released".into());
                    }
                }
            }
        }
        service.holders.retain(|id, holder| {
            let keep = wanted.contains_key(id);
            if let Some(file) = holder.file.as_ref().filter(|_| !keep) {
                // A file nobody refreshes must not pass for the current picture.
                let _ = std::fs::write(
                    file,
                    "# Herdr situation\n\nThe overseer role for this file has ended. It is no longer updated.\n",
                );
            }
            if let Some(kind) = wanted.get(id) {
                let changed_kind = *kind != holder.kind;
                holder.kind = *kind;
                if changed_kind {
                    holder.written_at = None;
                }
            }
            keep
        });
        for (id, kind) in wanted {
            service
                .holders
                .entry(id)
                .or_insert_with(|| Holder::new(kind, now));
        }
        if was_empty && !service.holders.is_empty() {
            service.previous.clear();
            service.turn_ends = self
                .state
                .terminals
                .iter()
                .map(|(id, t)| (id.clone(), t.turn_reporting.as_ref().map_or(0, |r| r.ends)))
                .collect();
        }
    }

    /// Who is told about a pane. A space overseer hears its own space. The
    /// chief hears every space that has no overseer of its own, set down
    /// agents, and the space overseers themselves.
    fn overseer_receivers(
        &self,
        target: &TerminalId,
        places: &HashMap<TerminalId, usize>,
    ) -> Vec<TerminalId> {
        let holders = &self.overseer_service.holders;
        let space = places.get(target).copied();
        let has_space_role = |id: &TerminalId| {
            self.state
                .terminals
                .get(id)
                .is_some_and(|terminal| terminal.overseer.is_space_overseer())
        };
        let has_session_role = |id: &TerminalId| {
            self.state
                .terminals
                .get(id)
                .is_some_and(|terminal| terminal.overseer.is_session_overlord())
        };
        let in_space = |id: &TerminalId| {
            has_space_role(id) && space.is_some() && places.get(id).copied() == space
        };
        let covered = holders.keys().any(in_space);
        let mut receivers: Vec<TerminalId> = holders
            .keys()
            .filter(|id| *id != target && in_space(id))
            .cloned()
            .collect();
        let is_space_overseer = has_space_role(target);
        if !covered || is_space_overseer {
            receivers.extend(
                holders
                    .keys()
                    .filter(|id| *id != target && has_session_role(id))
                    .cloned(),
            );
        }
        let mut seen = HashSet::new();
        receivers.retain(|id| seen.insert(id.clone()));
        receivers
    }

    /// Watch every pane once and queue what each overseer should hear.
    pub(super) fn collect_overseer_events(&mut self, now: Instant) {
        self.sync_overseer_holders(now);
        if self.overseer_service.holders.is_empty() {
            return;
        }
        let places = self.state.terminal_places();
        let dispatched = crate::overseer::dispatched_stages(&self.overseer_service.queue_path);
        let queue_markers = self
            .state
            .terminals
            .keys()
            .filter_map(|id| {
                let name = crate::pane_names::effective_name(&self.state.terminals, id)?;
                crate::overseer::queued_stage_marker(&self.overseer_service.queue_path, &name)
                    .map(|marker| (id.clone(), marker))
            })
            .collect::<HashMap<_, _>>();
        self.overseer_service
            .previous
            .retain(|id, _| self.state.terminals.contains_key(id));
        self.overseer_service
            .turn_ends
            .retain(|id, _| self.state.terminals.contains_key(id));
        self.overseer_service.direct_stops.retain(|id, stop| {
            self.state
                .terminals
                .get(id)
                .is_some_and(|t| t.job_marker.as_ref() == Some(&stop.marker))
        });
        let routes: HashMap<TerminalId, Vec<TerminalId>> = self
            .state
            .terminals
            .keys()
            .map(|id| (id.clone(), self.overseer_receivers(id, &places)))
            .collect();
        for (holder_id, holder) in self.overseer_service.holders.iter_mut() {
            holder.events.retain(|key, event| {
                key.starts_with(MAIL_PREFIX)
                    || self.state.terminals.iter().any(|(id, terminal)| {
                        routes.get(id).is_some_and(|r| r.contains(holder_id))
                            && ((id.to_string() == *key
                                && terminal.state == event.state
                                && blocked_reason(terminal) == event.reason)
                                || (format!("{id}:turn") == *key
                                    && (terminal.job_marker.is_none()
                                        || terminal.job_marker.as_ref() == event.marker.as_ref())
                                    && crate::overseer::task_end_with_queue_marker(
                                        event.marker.as_deref(),
                                        queue_markers.get(id).map(|marker| marker.as_path()),
                                        dispatched.contains_key(
                                            &crate::pane_names::effective_name(
                                                &self.state.terminals,
                                                id,
                                            )
                                            .unwrap_or_default(),
                                        ),
                                    )
                                    .is_some_and(|outcome| event.text.ends_with(outcome))))
                    })
            });
        }
        let many_spaces = self.state.workspaces.len() > 1;
        let mut queued: Vec<Queued> = Vec::new();
        let mut ended_assignments = Vec::new();
        let origin_updates: Vec<_> = self
            .state
            .terminals
            .iter()
            .filter_map(|(id, terminal)| {
                let origin = terminal.message_origin.as_ref()?;
                let session = terminal.current_session_identity_for_persistence();
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                if Some(origin.session.clone()) != session || origin.stale_pending(now) {
                    return Some((id.clone(), None));
                }
                let was_working = self
                    .overseer_service
                    .previous
                    .get(id)
                    .is_some_and(|(state, _)| *state == AgentState::Working);
                let was_observed = self.overseer_service.previous.contains_key(id);
                let hook_ended_after_submission = terminal
                    .turn_reporting
                    .as_ref()
                    .is_some_and(|report| report.ends != origin.submitted_turn_ends);
                // If no start was observed, an idle notification is either an
                // acknowledgement completed between ticks or a runtime that
                // has not exposed a provable start boundary. Fail open here so
                // it cannot claim the next unrelated completion.
                if origin.kind == MessageOriginKind::Notification
                    && !origin.active
                    && !origin.submitted_while_working
                    && terminal.state != AgentState::Working
                {
                    return Some((id.clone(), None));
                }
                // Native hook counters survive a complete start/end between
                // collector passes, so retire an unobserved notification at
                // its accepted lifecycle boundary too.
                if origin.kind == MessageOriginKind::Notification
                    && !origin.active
                    && hook_ended_after_submission
                {
                    return Some((id.clone(), None));
                }
                // If a sampled notification turn ended and the hook already
                // started another turn before this pass, detach the old origin
                // before the new turn can be mistaken for its acknowledgement.
                if origin.kind == MessageOriginKind::Notification
                    && origin.active
                    && hook_ended_after_submission
                    && terminal.state == AgentState::Working
                {
                    return Some((id.clone(), None));
                }
                // A cold restore can resume an agent after its prior turn
                // finished while Herdr was down. Do not carry that completed
                // turn's active correlation into the next request.
                if origin.active && !was_observed && terminal.state != AgentState::Working {
                    return Some((id.clone(), None));
                }
                if terminal.state == AgentState::Working
                    && !was_working
                    && !origin.active
                    && !origin.submitted_while_working
                {
                    let mut active = origin.clone();
                    active.active = true;
                    return Some((id.clone(), Some(active)));
                }
                None
            })
            .collect();
        if !origin_updates.is_empty() {
            for (id, origin) in origin_updates {
                if let Some(terminal) = self.state.terminals.get_mut(&id) {
                    terminal.message_origin = origin;
                }
            }
            self.state.mark_session_dirty();
            self.schedule_session_save();
        }
        let mut finished_origins = Vec::new();
        for (id, terminal) in &self.state.terminals {
            let current_reason = blocked_reason(terminal);
            let old = self
                .overseer_service
                .previous
                .insert(id.clone(), (terminal.state, current_reason.clone()));
            let ended = terminal.turn_reporting.as_ref().filter(|r| {
                r.ends != 0 && r.ends != *self.overseer_service.turn_ends.get(id).unwrap_or(&0)
            });
            // Record before comparing states: a question can end a turn while
            // the pane remains blocked or idle, with no screen transition.
            // A native turn also ends after a short reply. Only an assigned
            // stage expects a marker; queue-owned stages are reported once by
            // the dispatcher, including validation and stalled-stage alarms.
            let name =
                crate::pane_names::effective_name(&self.state.terminals, id).unwrap_or_default();
            // A plain question ends Idle too: keep its registration and wait
            // for continuous silence before reporting a real stop once.
            let stage_end = terminal.state == AgentState::Idle;
            let outcome = crate::overseer::task_end_with_queue_marker(
                terminal.job_marker.as_deref(),
                queue_markers.get(id).map(|marker| marker.as_path()),
                dispatched.contains_key(&name),
            );
            let mut reported_end = None;
            if stage_end && ended.is_some() && outcome == Some("finished") {
                reported_end = outcome;
                ended_assignments.push(id.clone());
                self.overseer_service.direct_stops.remove(id);
            } else if let (Some(marker), Some(report), Some("stopped without a hand-off")) = (
                terminal.job_marker.as_ref(),
                terminal.turn_reporting.as_ref(),
                outcome,
            ) {
                let stop = self
                    .overseer_service
                    .direct_stops
                    .entry(id.clone())
                    .or_insert_with(|| {
                        crate::overseer::DirectStop::new(marker.clone(), report.seq)
                    });
                if stop.observe(
                    report.seq,
                    stage_end && !report.working,
                    ended.is_some(),
                    now,
                ) {
                    reported_end = outcome;
                }
            } else {
                self.overseer_service.direct_stops.remove(id);
            }
            if let Some(r) = ended {
                self.overseer_service.turn_ends.insert(id.clone(), r.ends);
            }
            let (previous, reason) = old.unwrap_or((terminal.state, current_reason.clone()));
            let output = self
                .terminal_runtimes
                .get(id)
                .map(|rt| crate::overseer::tail(&rt.detection_text()))
                .unwrap_or_default();
            let event = if let Some(end) = reported_end {
                Some(end.to_string())
            } else if terminal.state == AgentState::Blocked && reason != current_reason {
                Some(format!(
                    "blocked: {}",
                    current_reason.as_deref().unwrap_or("needs attention")
                ))
            } else if terminal.turn_reporting.is_some() {
                None
            } else {
                crate::overseer::transition(
                    previous,
                    terminal.state,
                    current_reason.as_deref(),
                    &output,
                )
            };
            let current_session = terminal.current_session_identity_for_persistence();
            let turn_finished = terminal.state == AgentState::Idle && previous != AgentState::Idle;
            let event = event.and_then(|event| {
                terminal
                    .message_origin
                    .as_ref()
                    .filter(|origin| Some(origin.session.clone()) == current_session)
                    .map_or(Some(event.clone()), |origin| {
                        origin.result_event(id, &event)
                    })
            });
            if turn_finished {
                finished_origins.push(id.clone());
            }
            let Some(event) = event else {
                continue;
            };
            let receivers = routes.get(id).cloned().unwrap_or_default();
            if receivers.is_empty() {
                continue;
            }
            let name = crate::pane_names::effective_name(&self.state.terminals, id)
                .unwrap_or_else(|| id.to_string());
            let plain = format!("{} {event}", one_line(&name));
            // Only the chief needs to be told which space a line is about.
            let with_space = match places.get(id) {
                Some(index) if many_spaces => format!("{}: {plain}", self.space_label(*index)),
                _ => plain.clone(),
            };
            queued.push(Queued {
                receivers,
                key: if reported_end.is_some() {
                    format!("{id}:turn")
                } else {
                    id.to_string()
                },
                plain,
                with_space,
                state: terminal.state,
                reason: current_reason,
                marker: reported_end.and(terminal.job_marker.clone()),
            });
        }
        if !finished_origins.is_empty() {
            for id in finished_origins {
                if let Some(terminal) = self.state.terminals.get_mut(&id) {
                    terminal.message_origin = None;
                }
            }
            self.state.mark_session_dirty();
            self.schedule_session_save();
        }
        // Only a HANDOFF-complete turn consumes a registration. Real stops
        // retain it and use per-registration deduplication; later replies do
        // not produce another stop. Keep the path for pending-event rechecks.
        if !ended_assignments.is_empty() {
            for id in ended_assignments {
                if let Some(terminal) = self.state.terminals.get_mut(&id) {
                    terminal.job_marker = None;
                }
            }
            self.state.mark_session_dirty();
            self.schedule_session_save();
        }
        for queued in queued {
            for receiver in &queued.receivers {
                let Some(holder) = self.overseer_service.holders.get_mut(receiver) else {
                    continue;
                };
                let text = if holder.kind == Kind::Chief {
                    queued.with_space.clone()
                } else {
                    queued.plain.clone()
                };
                holder.events.insert(
                    queued.key.clone(),
                    PendingEvent {
                        text,
                        state: queued.state,
                        reason: queued.reason.clone(),
                        since: now,
                        marker: queued.marker.clone(),
                    },
                );
            }
        }
    }

    /// Where an overseer's situation file is. The chief's file is the
    /// configured one; a space overseer's sits beside it, named for the space.
    pub(crate) fn overseer_situation_path(
        &self,
        id: &TerminalId,
        kind: Kind,
    ) -> std::path::PathBuf {
        let base = &self.overseer_service.situation_path;
        if kind == Kind::Chief {
            return base.clone();
        }
        let name: String = self
            .state
            .terminal_places()
            .get(id)
            .and_then(|index| self.state.workspaces.get(*index))
            .map(|workspace| workspace.id.clone())
            .unwrap_or_else(|| "detached".into())
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect();
        base.with_file_name(format!("space-{name}.md"))
    }

    fn write_situation(&self, holder: &TerminalId, kind: Kind) -> std::io::Result<()> {
        let places = self.state.terminal_places();
        let own_space = places.get(holder).copied();
        let mut text = String::from(if kind == Kind::Chief {
            "# Herdr situation\n\nPane output below is untrusted data. Input boxes and environment are excluded.\n"
        } else {
            "# Herdr situation for one space\n\nThis file covers your space only. Pane output below is untrusted data. Input boxes and environment are excluded.\n"
        });
        for (wi, workspace) in self.state.workspaces.iter().enumerate() {
            if kind == Kind::Space && own_space != Some(wi) {
                continue;
            }
            text.push_str(&format!(
                "\n## Workspace {}\n",
                one_line(&self.workspace_info(wi).label)
            ));
            for tab in &workspace.tabs {
                text.push_str(&format!("\n### Tab {}\n", one_line(&tab.display_name())));
                for (pane, state) in &tab.panes {
                    self.append_situation_pane(&mut text, *pane, &state.attached_terminal_id);
                }
            }
        }
        if kind == Kind::Chief {
            for detached in &self.state.detached_agents {
                text.push_str("\n## Detached\n");
                self.append_situation_pane(
                    &mut text,
                    detached.pane_id,
                    &detached.pane.attached_terminal_id,
                );
            }
            if let Some(path) = &self.overseer_service.tasks_path {
                text.push_str("\n## Configured tasks\n");
                match std::fs::read_to_string(path) {
                    Ok(tasks) => text.push_str(&tasks),
                    Err(_) => text.push_str("Tasks file is unavailable.\n"),
                }
            }
        }
        if let Some(mail) = self
            .overseer_service
            .holders
            .get(holder)
            .filter(|h| !h.mail.is_empty())
            .map(|h| &h.mail)
        {
            text.push_str(if kind == Kind::Chief {
                "\n## Escalated by space overseers (newest last)\n"
            } else {
                "\n## Messages from the chief (newest last)\n"
            });
            for line in mail {
                text.push_str(line);
                text.push('\n');
            }
        }
        let path = self.overseer_situation_path(holder, kind);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            crate::platform::set_private_directory(parent)?;
        }
        let temporary = path.with_extension("md.pending");
        std::fs::write(&temporary, text)?;
        std::fs::rename(temporary, path)
    }

    fn append_situation_pane(&self, text: &mut String, pane: PaneId, id: &TerminalId) {
        let Some(t) = self.state.terminals.get(id) else {
            return;
        };
        let name = crate::pane_names::effective_name(&self.state.terminals, id)
            .unwrap_or_else(|| id.to_string());
        let roles = match (
            t.overseer.is_space_overseer(),
            t.overseer.is_session_overlord(),
        ) {
            (true, true) => "space overseer and session overlord",
            (true, false) => "space overseer",
            (false, true) => "session overlord",
            (false, false) => "none",
        };
        text.push_str(&format!(
            "\nPane {} ({pane:?}); agent: {}; state: {:?}; blocked: {}; model: {}; roles: {}\n",
            one_line(&name),
            t.effective_agent_label().unwrap_or("unknown"),
            t.state,
            blocked_reason(t).as_deref().unwrap_or("-"),
            t.model_info
                .as_ref()
                .map(|i| i.model.as_str())
                .or_else(|| t.model_pin.as_ref().map(|pin| pin.model.as_str()))
                .map(one_line)
                .unwrap_or_else(|| "unknown".into()),
            roles
        ));
        if let Some(runtime) = self.terminal_runtimes.get(id) {
            text.push_str(&crate::overseer::tail(&runtime.detection_text()));
            text.push('\n');
        }
    }

    /// Queue one line for an overseer. It is sent with the next batch and
    /// kept in the receiver's situation file.
    fn queue_overseer_mail(
        &mut self,
        from: &TerminalId,
        from_role: Kind,
        to: &TerminalId,
        to_role: Kind,
        direction: crate::overseer::MessageDirection,
        line: String,
        wake: bool,
        now: Instant,
    ) {
        self.overseer_service.mail_seq += 1;
        let key = self
            .overseer_service
            .message_id(self.overseer_service.mail_seq);
        self.overseer_service.message_deliveries.insert(
            key.clone(),
            crate::overseer::MessageDelivery {
                target_terminal_id: to.clone(),
                state: if wake {
                    crate::overseer::MessageDeliveryState::Queued
                } else {
                    crate::overseer::MessageDeliveryState::Deferred
                },
                sender_terminal_id: from.clone(),
                direction,
                sender_role: from_role,
                sender_role_generation: self
                    .state
                    .terminals
                    .get(from)
                    .map_or(0, |terminal| terminal.overseer.generation(from_role)),
                sender_session: self
                    .state
                    .terminals
                    .get(from)
                    .and_then(|terminal| terminal.current_session_identity_for_persistence()),
                receiver_session: self
                    .state
                    .terminals
                    .get(to)
                    .and_then(|terminal| terminal.current_session_identity_for_persistence()),
                receiver_role: to_role,
                receiver_role_generation: self
                    .state
                    .terminals
                    .get(to)
                    .map_or(0, |terminal| terminal.overseer.generation(to_role)),
                sender_workspace_id: self
                    .state
                    .terminal_places()
                    .get(from)
                    .and_then(|index| self.state.workspaces.get(*index))
                    .map(|workspace| workspace.id.clone()),
                hold_reason: None,
            },
        );
        let Some(holder) = self.overseer_service.holders.get_mut(to) else {
            return;
        };
        let identified = format!("[message_id={key}] {line}");
        holder.mail.push_back(identified.clone());
        while holder.mail.len() > crate::overseer::MAIL_LOG {
            holder.mail.pop_front();
        }
        holder.written_at = None;
        if !wake {
            return;
        }
        holder.events.insert(
            key,
            PendingEvent {
                text: identified,
                state: AgentState::Idle,
                reason: None,
                marker: None,
                // Sent lines wait for no settle time.
                since: now
                    .checked_sub(Duration::from_secs(
                        self.overseer_service.config.event_settle_seconds,
                    ))
                    .unwrap_or(now),
            },
        );
    }

    fn overseer_caller(
        &mut self,
        caller: PaneId,
        want: Kind,
        now: Instant,
    ) -> Result<TerminalId, Refusal> {
        self.sync_overseer_holders(now);
        let id = self
            .state
            .terminal_id_for_any_pane(caller)
            .ok_or(("pane_not_found", "calling pane not found".to_string()))?;
        let permitted = self
            .state
            .terminals
            .get(&id)
            .is_some_and(|terminal| match want {
                Kind::Space => terminal.overseer.is_space_overseer(),
                Kind::Chief => terminal.overseer.is_session_overlord(),
            });
        if permitted {
            return Ok(id);
        }
        Err((
            "not_allowed",
            match want {
                Kind::Space => "only a space overseer can escalate",
                Kind::Chief => "only the chief overseer can message a space overseer",
            }
            .to_string(),
        ))
    }

    fn overseer_name(&self, id: &TerminalId) -> String {
        one_line(
            &crate::pane_names::effective_name(&self.state.terminals, id)
                .unwrap_or_else(|| id.to_string()),
        )
    }

    /// A space overseer passes one line up to the chief. It can reach no
    /// other pane. Returns how many chiefs were told.
    pub(crate) fn overseer_escalate(
        &mut self,
        caller: PaneId,
        text: &str,
        wake: bool,
        now: Instant,
    ) -> Result<usize, Refusal> {
        let from = self.overseer_caller(caller, Kind::Space, now)?;
        let line =
            crate::overseer::mail_line(text).map_err(|(code, why)| (code, why.to_string()))?;
        let chiefs: Vec<TerminalId> = self
            .overseer_service
            .holders
            .keys()
            .filter(|id| {
                self.state
                    .terminals
                    .get(*id)
                    .is_some_and(|terminal| terminal.overseer.is_session_overlord())
            })
            .cloned()
            .collect();
        if chiefs.is_empty() {
            return Err((
                "no_chief",
                "no session overlord is assigned; use `herdr overlord set <pane>`; nothing was sent".to_string(),
            ));
        }
        if wake
            && chiefs.iter().any(|id| {
                id != &from
                    && self.overseer_service.holders[id].mail_waiting()
                        >= crate::overseer::MAIL_LIMIT
            })
        {
            return Err((
                "mailbox_full",
                "the chief has unread lines waiting; try again later".to_string(),
            ));
        }
        let space = self
            .state
            .terminal_places()
            .get(&from)
            .map(|index| self.space_label(*index))
            .unwrap_or_default();
        let text = format!(
            "{} (space {space}) escalates: {line}",
            self.overseer_name(&from)
        );
        for chief in &chiefs {
            let self_route = chief == &from;
            let message = if self_route {
                format!("{text} (self route; informational only)")
            } else {
                text.clone()
            };
            self.queue_overseer_mail(
                &from,
                Kind::Space,
                chief,
                Kind::Chief,
                crate::overseer::MessageDirection::Escalation,
                message,
                wake && !self_route,
                now,
            );
        }
        Ok(chiefs.len())
    }

    /// The chief sends one line to a space overseer, and only to one.
    pub(crate) fn overseer_message(
        &mut self,
        caller: PaneId,
        target: PaneId,
        text: &str,
        now: Instant,
    ) -> Result<(), Refusal> {
        let from = self.overseer_caller(caller, Kind::Chief, now)?;
        let line =
            crate::overseer::mail_line(text).map_err(|(code, why)| (code, why.to_string()))?;
        let to = self
            .state
            .terminal_id_for_any_pane(target)
            .filter(|id| {
                self.state
                    .terminals
                    .get(id)
                    .is_some_and(|terminal| terminal.overseer.is_space_overseer())
            })
            .ok_or((
                "not_a_space_overseer",
                "the target is not a space overseer".to_string(),
            ))?;
        if self.overseer_service.holders[&to].mail_waiting() >= crate::overseer::MAIL_LIMIT {
            return Err((
                "mailbox_full",
                "that overseer has unread lines waiting; try again later".to_string(),
            ));
        }
        let self_route = to == from;
        let text = if self_route {
            format!(
                "chief {} says: {line} (self route; informational only)",
                self.overseer_name(&from)
            )
        } else {
            format!("chief {} says: {line}", self.overseer_name(&from))
        };
        self.queue_overseer_mail(
            &from,
            Kind::Chief,
            &to,
            Kind::Space,
            crate::overseer::MessageDirection::Downward,
            text,
            !self_route,
            now,
        );
        Ok(())
    }

    /// Return a refusal in the role note before issuing any quit command.
    /// A supported resume identity and a runnable launcher are mandatory.
    fn overseer_profile(
        &mut self,
        id: &TerminalId,
        active: bool,
        now: Instant,
    ) -> Result<Option<bool>, String> {
        let t = &self.state.terminals[id];
        let agent = t.effective_agent_label().unwrap_or("");
        let config = &self.overseer_service.config;
        let extra = match agent {
            "claude" => config.claude_settings.as_ref().map(|path| {
                vec![
                    "--settings".into(),
                    crate::overseer::expand_path(path).display().to_string(),
                ]
            }),
            "codex" => config.codex_args.clone(),
            "cursor" => config.cursor_args.clone(),
            _ => None,
        };
        if extra.is_none() && t.overseer.profile_base.is_none() {
            return Ok(Some(false));
        }
        if self.no_session {
            return Err(
                "permission profile needs the Herdr server; it was not applied".to_string(),
            );
        }
        let refusal = || {
            "Permission profile was not changed: no verified resume command; the agent is still running.".to_string()
        };
        let mut base = if let Some(base) = &t.overseer.profile_base {
            base.clone()
        } else {
            let (source, label, kind, value) = t
                .current_session_identity_for_persistence()
                .ok_or_else(refusal)?;
            let session = crate::agent_resume::AgentSessionRef { kind, value };
            let plan = crate::agent_resume::plan(&source, &label, &session).ok_or_else(refusal)?;
            let mut argv = crate::agent_model::apply_pin_to_argv(&plan.argv, t.model_pin.as_ref());
            // Resume preserves conversation; explicitly retain an observed model.
            if t.model_pin.is_none() {
                let model = t.model_info.as_ref().ok_or_else(refusal)?;
                argv.extend(["--model".into(), model.model.clone()]);
            }
            argv
        };
        if agent == "claude" && active {
            if let Some(path) = &config.claude_settings {
                if !crate::overseer::expand_path(path).is_file() {
                    return Err("Permission profile was not changed: settings file is unavailable; the agent is still running.".into());
                }
            }
        }
        if active
            && extra.as_ref().is_some_and(|args| {
                args.iter().any(|arg| {
                    matches!(
                        arg.as_str(),
                        "resume"
                            | "--resume"
                            | "--continue"
                            | "--model"
                            | "-m"
                            | "--print"
                            | "-p"
                            | "exec"
                            | "--help"
                            | "--version"
                    ) || arg.starts_with("--model=")
                        || arg.starts_with("--resume=")
                })
            })
        {
            return Err("Permission profile was not changed: profile arguments must preserve the conversation and model; the agent is still running.".into());
        }
        let original = base.clone();
        if active {
            base.extend(extra.unwrap_or_default());
        }
        base.first().ok_or_else(refusal)?;
        if !self.overseer_ready(id, now) {
            self.overseer_service.profile_probes.remove(id);
            return Ok(None);
        }
        if self.overseer_service.profile_probes.remove(id).as_ref() != Some(&base) {
            if !launcher_works(&base) {
                return Err(refusal());
            }
            self.overseer_service
                .profile_probes
                .insert(id.clone(), base);
            // Never quit in the call that blocked on the launcher. Client keys
            // received during that probe are still waiting in the server queue.
            return Ok(None);
        }
        let quit = super::agent_restart::quit_command(agent).ok_or_else(refusal)?;
        let runtime = self.terminal_runtimes.get(id).ok_or_else(refusal)?;
        if super::api_helpers::send_prompt(runtime, quit).is_err() {
            return Err(refusal());
        }
        if let Some(t) = self.state.terminals.get_mut(id) {
            t.overseer.profile_base = Some(original);
            t.overseer.restart_argv = Some(base.clone());
            t.pending_restart = Some(super::agent_restart::AgentRestart {
                argv: base,
                started: now,
                shell_free_since: None,
            });
        }
        Ok(Some(true))
    }
}

fn blocked_reason(terminal: &crate::terminal::TerminalState) -> Option<String> {
    terminal.blocked_reason.map(str::to_string).or_else(|| {
        terminal
            .hook_authority
            .as_ref()
            .filter(|hook| hook.state == AgentState::Blocked)
            .and_then(|hook| hook.message.as_ref())
            .map(|message| one_line(message))
    })
}

fn routing_block_status(terminal: &crate::terminal::TerminalState, id: &TerminalId) -> String {
    let path = std::env::var_os("TEAM_ROUTING_CONFIG")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| {
                std::path::PathBuf::from(home).join(".config/herdr/team-message-routing.json")
            })
        });
    let Some(path) = path else {
        return "unknown (HOME unavailable; routing policy not read)".into();
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return format!("unknown (routing policy unavailable at {})", path.display());
    };
    let Ok(policy) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return format!(
            "invalid (routing policy is malformed at {})",
            path.display()
        );
    };
    let agent = terminal
        .effective_known_agent()
        .map(crate::detect::agent_label)
        .unwrap_or("unknown");
    let blocked = policy
        .get("blocked_agents")
        .and_then(|v| v.as_array())
        .map(|agents| {
            agents
                .iter()
                .filter_map(|v| v.as_str())
                .any(|v| v.eq_ignore_ascii_case(agent))
        })
        .unwrap_or(agent == "claude");
    let rule = policy
        .get("terminal_exceptions")
        .and_then(|v| v.get(id.to_string()));
    if !blocked && rule.is_none() {
        return format!("not blocked by policy at {}", path.display());
    }
    let Some(rule) = rule else {
        return format!("blocked; no terminal exception in {}", path.display());
    };
    let Some(start) = rule.get("starts_at").and_then(serde_json::Value::as_f64) else {
        return format!(
            "invalid (exception has no numeric starts_at in {})",
            path.display()
        );
    };
    let Some(end) = rule.get("expires_at").and_then(serde_json::Value::as_f64) else {
        return format!(
            "invalid (exception has no numeric expires_at in {})",
            path.display()
        );
    };
    let session = terminal
        .persisted_agent_session
        .as_ref()
        .map(|s| s.session_ref.value.as_str());
    let policy_session = rule
        .get("agent_session")
        .and_then(serde_json::Value::as_str);
    let policy_agent = rule.get("agent").and_then(serde_json::Value::as_str);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    if !valid_routing_exception_window(start, end) {
        return format!(
            "invalid (exception duration must be at most 3600s; expires_at={end}; policy={})",
            path.display()
        );
    }
    if policy_agent != Some(agent) || session.is_none() || policy_session != session {
        return format!(
            "invalid (agent or native conversation does not match; expires_at={end}; policy={})",
            path.display()
        );
    }
    if !(start <= now && now < end) {
        return format!(
            "blocked (exception expired or not yet active; expires_at={end}; policy={})",
            path.display()
        );
    }
    format!(
        "{}exception active until {end}; policy={}",
        if blocked { "" } else { "not blocked; " },
        path.display()
    )
}

fn valid_routing_exception_window(start: f64, end: f64) -> bool {
    0.0 < end - start && end - start <= 3600.0
}

fn team_bench_status(name: &str) -> String {
    let path = std::env::var_os("TEAM_BENCHED")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| std::path::PathBuf::from(home).join("model-bench-runs/team/BENCHED"))
        });
    let Some(path) = path else {
        return "unknown (HOME unavailable; BENCHED not read)".into();
    };
    match std::fs::read_to_string(&path) {
        Ok(contents) if contents.lines().any(|line| line.trim() == name) => format!(
            "benched; expiry=not recorded in {}; remove name to release",
            path.display()
        ),
        Ok(_) => format!(
            "not benched in {} (entries have no expiry field)",
            path.display()
        ),
        Err(_) => format!("unknown (BENCHED unavailable at {})", path.display()),
    }
}

fn team_stage_finishes(queue: &std::path::Path, pane: &str) -> String {
    let Ok(contents) = std::fs::read_to_string(queue) else {
        return format!("unknown (could not read {})", queue.display());
    };
    let mut rows: Vec<_> = contents
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let fields: Vec<_> = line.split('\t').collect();
            (fields.len() > 13 && fields[1] == pane && fields[8] == "done").then(|| {
                format!(
                    "{} title={} out={} done_at={}",
                    fields[0],
                    one_line(fields[10]),
                    fields[4],
                    fields[13]
                )
            })
        })
        .collect();
    if rows.is_empty() {
        format!(
            "none (looked for completed rows for {pane} in {})",
            queue.display()
        )
    } else {
        if rows.len() > 3 {
            rows.drain(..rows.len() - 3);
        }
        rows.join(" | ")
    }
}

#[cfg(test)]
mod scoped_view_tests {
    use super::{team_stage_finishes, valid_routing_exception_window};

    #[test]
    fn scoped_view_keeps_recent_finished_stages_with_declared_output() {
        let queue = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/app/fixtures/team-view-queue.tsv");
        let finishes = team_stage_finishes(&queue, "Worker");
        assert!(finishes.contains("alpha title=Alpha finished out=A.done done_at=2"));
        assert!(!finishes.contains("beta"));
    }

    #[test]
    fn routing_exception_window_rejects_long_and_nonpositive_durations() {
        assert!(valid_routing_exception_window(100.0, 3700.0));
        assert!(!valid_routing_exception_window(100.0, 3700.1));
        assert!(!valid_routing_exception_window(100.0, 100.0));
    }
}

fn one_line(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(240)
        .collect()
}

fn launcher_works(argv: &[String]) -> bool {
    let Some((program, args)) = argv.split_first() else {
        return false;
    };
    use std::process::{Command, Stdio};
    let Ok(mut child) = Command::new(program)
        .args(args)
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::Agent;
    use crate::overseer::{Kind, Scope};
    use crate::terminal::{TerminalRuntime, TerminalState};
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::layout::Rect;
    use std::collections::HashMap;

    fn setup() -> (
        App,
        PaneId,
        TerminalId,
        tokio::sync::mpsc::Receiver<bytes::Bytes>,
    ) {
        let (_, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state = AppState::test_new();
        let ws = crate::workspace::Workspace::test_new("Test space");
        let pane = ws.tabs[0].root_pane;
        let id = ws.terminal_id(pane).unwrap().clone();
        let mut t = TerminalState::new(id.clone(), std::env::temp_dir());
        t.detected_agent = Some(Agent::Codex);
        t.state = AgentState::Idle;
        t.agent_name = Some("reviewer".into());
        let (rt, rx) = TerminalRuntime::test_with_channel(80, 24);
        rt.test_process_pty_bytes(b"checks passed\r\n\xe2\x80\xba \r\n");
        app.terminal_runtimes.insert(id.clone(), rt);
        app.state.terminals.insert(id.clone(), t);
        app.state.workspaces.push(ws);
        app.state.active = Some(0);
        app.state.mode = crate::app::Mode::Terminal;
        let folder = std::env::temp_dir().join(format!("herdr-overseer-{}", TerminalId::alloc()));
        app.overseer_service.situation_path = folder.join("situation.md");
        app.overseer_service.queue_path = folder.join("queue.tsv");
        (app, pane, id, rx)
    }

    fn collector_worker(app: &mut App, chief_pane: PaneId, now: Instant) -> (PaneId, TerminalId) {
        app.state.toggle_oversee(chief_pane);
        let worker_ws = crate::workspace::Workspace::test_new("Worker space");
        let worker_pane = worker_ws.tabs[0].root_pane;
        let worker_id = worker_ws.terminal_id(worker_pane).unwrap().clone();
        let mut worker = TerminalState::new(worker_id.clone(), std::env::temp_dir());
        worker.detected_agent = Some(Agent::Codex);
        worker.agent_name = Some("worker".into());
        worker.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
            source: "herdr:codex".into(),
            agent: "codex".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id("worker-session").unwrap(),
        });
        let (runtime, _rx) = TerminalRuntime::test_with_channel(80, 24);
        app.terminal_runtimes.insert(worker_id.clone(), runtime);
        app.state.terminals.insert(worker_id.clone(), worker);
        app.state.workspaces.push(worker_ws);
        app.collect_overseer_events(now);
        (worker_pane, worker_id)
    }

    fn api_origin(
        app: &App,
        pane: PaneId,
        origin: &str,
        correlation: &str,
        task_id: Option<&str>,
    ) -> MessageOrigin {
        app.message_origin_for_api(pane, Some(origin), Some(correlation), task_id)
            .unwrap()
            .unwrap()
    }

    fn collected_text(app: &App, chief: &TerminalId, key: &str) -> Option<String> {
        app.overseer_service
            .holders
            .get(chief)
            .and_then(|holder| holder.events.get(key))
            .map(|event| event.text.clone())
    }

    #[tokio::test]
    async fn collector_clears_missed_notification_before_same_session_followup() {
        let (mut app, chief_pane, chief_id, _rx) = setup();
        let now = Instant::now();
        let (worker_pane, worker_id) = collector_worker(&mut app, chief_pane, now);
        let origin = api_origin(&app, worker_pane, "notification", "notice-missed", None);
        app.store_message_origin(worker_pane, Some(origin));

        // The whole notification reply occurs between collector passes.
        let worker = app.state.terminals.get_mut(&worker_id).unwrap();
        worker.state = AgentState::Working;
        worker.state = AgentState::Idle;
        app.collect_overseer_events(now + Duration::from_secs(1));
        assert!(app.state.terminals[&worker_id].message_origin.is_none());
        assert!(collected_text(&app, &chief_id, &worker_id.to_string()).is_none());

        // A later ordinary completion in the same native session is reported once.
        app.state.terminals.get_mut(&worker_id).unwrap().state = AgentState::Working;
        app.collect_overseer_events(now + Duration::from_secs(2));
        app.state.terminals.get_mut(&worker_id).unwrap().state = AgentState::Idle;
        app.collect_overseer_events(now + Duration::from_secs(3));
        let text = collected_text(&app, &chief_id, &worker_id.to_string()).unwrap();
        assert!(text.ends_with("done"), "{text}");
        app.collect_overseer_events(now + Duration::from_secs(4));
        assert_eq!(
            collected_text(&app, &chief_id, &worker_id.to_string()),
            Some(text)
        );
    }

    #[tokio::test]
    async fn collector_does_not_consume_turn_already_working_at_notification_send() {
        let (mut app, chief_pane, chief_id, _rx) = setup();
        let now = Instant::now();
        let (worker_pane, worker_id) = collector_worker(&mut app, chief_pane, now);
        app.state.terminals.get_mut(&worker_id).unwrap().state = AgentState::Working;
        app.collect_overseer_events(now + Duration::from_secs(1));
        let origin = api_origin(&app, worker_pane, "notification", "notice-raced", None);
        assert!(origin.submitted_while_working);
        app.store_message_origin(worker_pane, Some(origin));
        app.collect_overseer_events(now + Duration::from_secs(2));
        app.state.terminals.get_mut(&worker_id).unwrap().state = AgentState::Idle;
        app.collect_overseer_events(now + Duration::from_secs(3));
        let text = collected_text(&app, &chief_id, &worker_id.to_string()).unwrap();
        assert!(text.ends_with("done"), "{text}");
        assert!(app.state.terminals[&worker_id].message_origin.is_none());
    }

    #[tokio::test]
    async fn collector_uses_native_end_counter_for_unobserved_notification_ack() {
        use crate::turn_reporting::TurnEvent;
        let (mut app, chief_pane, _chief_id, _rx) = setup();
        let now = Instant::now();
        let (worker_pane, worker_id) = collector_worker(&mut app, chief_pane, now);
        app.state.terminals.get_mut(&worker_id).unwrap().state = AgentState::Working;
        app.collect_overseer_events(now + Duration::from_secs(1));
        let origin = api_origin(&app, worker_pane, "notification", "notice-native", None);
        assert!(origin.submitted_while_working);
        app.store_message_origin(worker_pane, Some(origin));
        let worker = app.state.terminals.get_mut(&worker_id).unwrap();
        worker.report_turn(Agent::Codex, TurnEvent::Ended, 1, Some("ack".into()));
        app.collect_overseer_events(now + Duration::from_secs(2));
        assert!(app.state.terminals[&worker_id].message_origin.is_none());
    }

    #[tokio::test]
    async fn collector_detaches_sampled_native_ack_before_a_followup_turn() {
        use crate::turn_reporting::TurnEvent;
        let (mut app, chief_pane, _chief_id, _rx) = setup();
        let now = Instant::now();
        let (worker_pane, worker_id) = collector_worker(&mut app, chief_pane, now);
        let origin = api_origin(
            &app,
            worker_pane,
            "notification",
            "notice-native-sampled",
            None,
        );
        app.store_message_origin(worker_pane, Some(origin));
        let worker = app.state.terminals.get_mut(&worker_id).unwrap();
        worker.report_turn(Agent::Codex, TurnEvent::Started, 1, Some("ack".into()));
        app.collect_overseer_events(now + Duration::from_secs(1));
        assert!(app.state.terminals[&worker_id]
            .message_origin
            .as_ref()
            .is_some_and(|origin| origin.active));

        let worker = app.state.terminals.get_mut(&worker_id).unwrap();
        worker.report_turn(Agent::Codex, TurnEvent::Ended, 2, Some("ack".into()));
        worker.report_turn(
            Agent::Codex,
            TurnEvent::Started,
            3,
            Some("follow-up".into()),
        );
        app.collect_overseer_events(now + Duration::from_secs(2));
        assert!(app.state.terminals[&worker_id].message_origin.is_none());
    }

    #[tokio::test]
    async fn collector_suppresses_sampled_notice_then_keeps_checked_task_and_failures() {
        let (mut app, chief_pane, chief_id, _rx) = setup();
        let now = Instant::now();
        let (worker_pane, worker_id) = collector_worker(&mut app, chief_pane, now);
        let origin = api_origin(&app, worker_pane, "notification", "notice-sampled", None);
        app.store_message_origin(worker_pane, Some(origin));
        app.state.terminals.get_mut(&worker_id).unwrap().state = AgentState::Working;
        app.collect_overseer_events(now + Duration::from_secs(1));
        assert!(app.state.terminals[&worker_id]
            .message_origin
            .as_ref()
            .is_some_and(|origin| origin.active));
        app.state.terminals.get_mut(&worker_id).unwrap().state = AgentState::Idle;
        app.collect_overseer_events(now + Duration::from_secs(2));
        assert!(collected_text(&app, &chief_id, &worker_id.to_string()).is_none());
        assert!(app.state.terminals[&worker_id].message_origin.is_none());

        let task = api_origin(
            &app,
            worker_pane,
            "task",
            "task-correlation",
            Some("task-7"),
        );
        app.store_message_origin(worker_pane, Some(task));
        app.state.terminals.get_mut(&worker_id).unwrap().state = AgentState::Working;
        app.collect_overseer_events(now + Duration::from_secs(3));
        app.state.terminals.get_mut(&worker_id).unwrap().state = AgentState::Idle;
        app.collect_overseer_events(now + Duration::from_secs(4));
        let text = collected_text(&app, &chief_id, &worker_id.to_string()).unwrap();
        assert!(text.contains("task_id=task-7"), "{text}");
        assert!(text.contains("correlation=task-correlation"), "{text}");
        assert!(app.state.terminals[&worker_id].message_origin.is_none());

        let failure = api_origin(&app, worker_pane, "notification", "notice-error", None);
        app.store_message_origin(worker_pane, Some(failure));
        app.state.terminals.get_mut(&worker_id).unwrap().state = AgentState::Working;
        app.collect_overseer_events(now + Duration::from_secs(5));
        app.state
            .terminals
            .get_mut(&worker_id)
            .unwrap()
            .blocked_reason = Some("approval");
        app.state.terminals.get_mut(&worker_id).unwrap().state = AgentState::Blocked;
        app.collect_overseer_events(now + Duration::from_secs(6));
        assert!(collected_text(&app, &chief_id, &worker_id.to_string())
            .unwrap()
            .ends_with("blocked: approval"));

        app.state.terminals.get_mut(&worker_id).unwrap().state = AgentState::Working;
        app.state
            .terminals
            .get_mut(&worker_id)
            .unwrap()
            .blocked_reason = None;
        let error_origin = api_origin(&app, worker_pane, "notification", "notice-idle-error", None);
        app.store_message_origin(worker_pane, Some(error_origin));
        app.collect_overseer_events(now + Duration::from_secs(7));
        app.terminal_runtimes
            .get(&worker_id)
            .unwrap()
            .test_process_pty_bytes(b"\x1b[2J\x1b[Herror: tool failed\r\n\xe2\x9d\xaf ");
        assert!(app
            .terminal_runtimes
            .get(&worker_id)
            .unwrap()
            .detection_text()
            .contains("error: tool failed"));
        app.state.terminals.get_mut(&worker_id).unwrap().state = AgentState::Idle;
        app.collect_overseer_events(now + Duration::from_secs(8));
        let error_text = collected_text(&app, &chief_id, &worker_id.to_string()).unwrap();
        assert!(error_text.ends_with("idle with an error"), "{error_text}");
    }

    #[tokio::test]
    async fn actual_headless_keyboard_enter_clears_only_pending_notification_origin() {
        let (mut app, pane, id, _rx) = setup();
        let terminal = app.state.terminals.get_mut(&id).unwrap();
        terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
            source: "herdr:codex".into(),
            agent: "codex".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id("keyboard-session").unwrap(),
        });
        let mut notification = api_origin(&app, pane, "notification", "keyboard-notice", None);
        notification.active = true;
        app.store_message_origin(pane, Some(notification));
        app.route_client_input(b"\r".to_vec());
        assert!(app.state.terminals[&id].message_origin.is_none());

        let task = api_origin(&app, pane, "task", "keyboard-task", Some("task-8"));
        app.store_message_origin(pane, Some(task));
        app.clear_pending_notification_origin(pane);
        assert_eq!(
            app.state.terminals[&id]
                .message_origin
                .as_ref()
                .map(|origin| origin.task_id.as_deref()),
            Some(Some("task-8"))
        );
    }

    async fn submitted(rx: &mut tokio::sync::mpsc::Receiver<bytes::Bytes>) -> String {
        let mut result = String::new();
        loop {
            let bytes = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            result.push_str(&String::from_utf8_lossy(&bytes));
            if bytes.ends_with(b"\r") {
                return result;
            }
        }
    }

    fn clear(app: &App, id: &TerminalId) {
        app.terminal_runtimes
            .get(id)
            .unwrap()
            .test_process_pty_bytes(b"\x1b[2J\x1b[Hchecks passed\r\n\xe2\x80\xba \r\n");
    }

    #[tokio::test]
    async fn overseer_click_key_one_holder_and_api_read_only() {
        let (mut app, pane, id, _rx) = setup();
        app.state
            .view
            .pane_chrome_controls
            .push(crate::app::state::PaneChromeControl {
                pane_id: pane,
                action: crate::app::state::PaneChromeAction::Oversee,
                rect: Rect::new(70, 1, 8, 1),
            });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 72,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.state.terminals[&id].overseer.active);
        app.state.mode = crate::app::Mode::Prefix;
        app.handle_prefix_key(KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT).into());
        assert!(!app.state.terminals[&id].overseer.active);
        let other_pane = PaneId::alloc();
        let other_id = TerminalId::alloc();
        app.state.terminals.insert(
            other_id.clone(),
            TerminalState::new(other_id.clone(), std::env::temp_dir()),
        );
        app.state
            .detached_agents
            .push(crate::app::state::DetachedAgent {
                pane_id: other_pane,
                pane: crate::pane::PaneState::new(other_id.clone()),
            });
        app.state.toggle_oversee(pane);
        app.state.toggle_oversee(other_pane);
        assert!(!app.state.terminals[&id].overseer.active);
        assert!(app.state.terminals[&other_id].overseer.active);
        app.state.toggle_oversee(pane);
        assert_eq!(
            serde_json::to_value(app.pane_info(0, pane).unwrap()).unwrap()["overseer"],
            "chief"
        );
        assert!(serde_json::from_str::<crate::api::schema::Request>(
            r#"{"id":"no","method":"pane.set_overseer","params":{"overseer":true}}"#
        )
        .is_err());
    }

    #[tokio::test]
    async fn independent_role_takeover_release_and_move_reconciliation() {
        let (mut app, first_pane, first_id, _rx) = setup();
        let second_ws = crate::workspace::Workspace::test_new("Other space");
        let second_pane = second_ws.tabs[0].root_pane;
        let second_id = second_ws.terminal_id(second_pane).unwrap().clone();
        app.state.terminals.insert(
            second_id.clone(),
            TerminalState::new(second_id.clone(), std::env::temp_dir()),
        );
        app.state.workspaces.push(second_ws);

        assert!(app.state.set_space_overseer(&first_id, true));
        assert!(app.state.set_session_overlord(&first_id, true));
        assert!(app.state.terminals[&first_id].overseer.is_space_overseer());
        assert!(app.state.terminals[&first_id]
            .overseer
            .is_session_overlord());

        assert!(app.state.set_space_overseer(&second_id, true));
        assert!(app.state.terminals[&first_id].overseer.is_space_overseer());
        assert!(app.state.terminals[&first_id]
            .overseer
            .is_session_overlord());
        assert!(app.state.set_session_overlord(&second_id, true));
        assert!(!app.state.terminals[&first_id]
            .overseer
            .is_session_overlord());
        assert!(app.state.terminals[&second_id].overseer.is_space_overseer());
        assert!(app.state.terminals[&second_id]
            .overseer
            .is_session_overlord());
        assert!(app.state.set_session_overlord(&second_id, false));
        assert!(app.state.terminals[&second_id].overseer.is_space_overseer());
        assert!(!app.state.terminals[&second_id]
            .overseer
            .is_session_overlord());
        let revision = app.state.terminals[&second_id].overseer.revision;
        assert!(!app.state.set_session_overlord(&second_id, false));
        assert_eq!(app.state.terminals[&second_id].overseer.revision, revision);

        assert!(!app.state.set_space_overseer(&first_id, true));
        assert!(app.state.move_pane_to_pane(
            first_pane,
            second_pane,
            crate::layout::DropZone::Over,
        ));
        assert!(!app.state.terminals[&first_id].overseer.is_space_overseer());
        assert!(app.state.terminals[&second_id].overseer.is_space_overseer());
    }

    #[tokio::test]
    async fn overseer_note_once_draft_quiet_period_and_off() {
        let (mut app, pane, id, mut rx) = setup();
        let now = Instant::now();
        app.state.toggle_oversee(pane);
        app.terminal_runtimes
            .get(&id)
            .unwrap()
            .test_process_pty_bytes(
                b"\x1b[2J\x1b[Hdone\r\n\xe2\x80\xba draft\r\nwrapped secret\r\n",
            );
        app.advance_overseer(now);
        assert!(rx.try_recv().is_err());
        assert_eq!(app.state.terminals[&id].overseer.notices.len(), 1);
        clear(&app, &id);
        app.terminal_runtimes.get(&id).unwrap().record_input();
        app.advance_overseer(now + Duration::from_secs(1));
        assert!(rx.try_recv().is_err());
        app.advance_overseer(now + Duration::from_secs(3));
        let note = submitted(&mut rx).await;
        assert!(note.contains("You are the overseer"));
        assert!(note.contains("situation.md"));
        app.advance_overseer(now + Duration::from_secs(5));
        assert!(rx.try_recv().is_err());
        app.state.toggle_oversee(pane);
        app.advance_overseer(now + Duration::from_secs(7));
        assert!(submitted(&mut rx).await.contains("role has ended"));
        assert!(app.state.terminals[&id].overseer.notices.is_empty());
    }

    #[tokio::test]
    async fn overseer_deleting_draft_waits_for_empty_prompt_and_paste_quiet() {
        for deletion in [b"\x08".as_slice(), b"\x7f", b"\x1b[3~"] {
            let (mut app, pane, id, mut rx) = setup();
            app.state.toggle_oversee(pane);
            let rt = &app.terminal_runtimes.get(&id).unwrap();
            rt.send_paste("draft".into()).await.unwrap();
            assert_eq!(rx.recv().await.unwrap().as_ref(), b"draft");
            assert!(rt.has_pending_input());
            assert!(!rt.input_is_quiet(Instant::now() + Duration::from_secs(3)));
            rt.record_input_bytes(deletion);
            assert!(!rt.has_pending_input());
            assert!(!rt.input_is_quiet(Instant::now()));
            rt.test_process_pty_bytes(b"\x1b[2J\x1b[Hdone\r\n\xe2\x80\xba dra\r\n");
            let now = Instant::now();
            app.advance_overseer(now + Duration::from_secs(3));
            assert!(rx.try_recv().is_err(), "partial deletion must keep waiting");
            clear(&app, &id);
            app.advance_overseer(now + Duration::from_secs(5));
            assert!(submitted(&mut rx).await.contains("You are the overseer"));
        }
        let (rt, mut rx) = TerminalRuntime::test_with_channel(80, 24);
        rt.send_paste("draft".into()).await.unwrap();
        rx.recv().await.unwrap();
        rt.record_input_bytes(b"\x7f");
        assert!(rt.input_is_quiet(Instant::now() + Duration::from_secs(3)));
        // Even a control-only paste must restart the quiet interval.
        rt.send_paste("\x15".into()).await.unwrap();
        assert!(!rt.input_is_quiet(Instant::now()));
        assert!(rt.input_is_quiet(Instant::now() + Duration::from_secs(3)));
    }

    #[tokio::test]
    async fn overseer_situation_tasks_metadata_and_input_exclusion() {
        let (mut app, pane, id, _rx) = setup();
        let tasks = app
            .overseer_service
            .situation_path
            .with_file_name("tasks.json");
        std::fs::create_dir_all(tasks.parent().unwrap()).unwrap();
        std::fs::write(&tasks, r#"{"tasks":[{"title":"Check the real build"}]}"#).unwrap();
        app.overseer_service.tasks_path = Some(tasks);
        app.state.terminals.get_mut(&id).unwrap().blocked_reason = Some("usage limit");
        app.terminal_runtimes
            .get(&id)
            .unwrap()
            .test_process_pty_bytes(
                b"\x1b[2J\x1b[Hchecks passed\r\n\xe2\x80\xba SECRET_DRAFT\r\nWRAPPED_SECRET\r\n",
            );
        app.state.toggle_oversee(pane);
        let now = Instant::now();
        app.advance_overseer(now);
        let report = std::fs::read_to_string(&app.overseer_service.situation_path).unwrap();
        for expected in [
            "Test space",
            "reviewer",
            "codex",
            "usage limit",
            "Check the real build",
            "checks passed",
        ] {
            assert!(report.contains(expected), "{expected}: {report}");
        }
        assert!(!report.contains("SECRET"));
        assert!(!report.contains("HOME="));
        let stamp = std::fs::metadata(&app.overseer_service.situation_path)
            .unwrap()
            .modified()
            .unwrap();
        app.advance_overseer(now + Duration::from_secs(2));
        assert_eq!(
            stamp,
            std::fs::metadata(&app.overseer_service.situation_path)
                .unwrap()
                .modified()
                .unwrap()
        );
    }

    #[tokio::test]
    async fn overseer_batches_events_and_stops_when_off() {
        let (mut app, pane, _id, mut rx) = setup();
        let other = TerminalId::alloc();
        let mut t = TerminalState::new(other.clone(), std::env::temp_dir());
        t.state = AgentState::Working;
        t.agent_name = Some("worker".into());
        app.state.terminals.insert(other.clone(), t);
        let now = Instant::now();
        app.state.toggle_oversee(pane);
        app.advance_overseer(now);
        assert!(submitted(&mut rx).await.contains("overseer"));
        app.state.terminals.get_mut(&other).unwrap().state = AgentState::Blocked;
        app.state.terminals.get_mut(&other).unwrap().blocked_reason = Some("usage limit");
        app.advance_overseer(now + Duration::from_secs(1));
        assert!(rx.try_recv().is_err());
        app.advance_overseer(now + Duration::from_secs(19));
        assert!(rx.try_recv().is_err());
        app.advance_overseer(now + Duration::from_secs(20));
        assert!(rx.try_recv().is_err());
        app.advance_overseer(now + Duration::from_secs(21));
        assert!(submitted(&mut rx)
            .await
            .contains("worker blocked: usage limit"));
        app.advance_overseer(now + Duration::from_secs(40));
        assert!(rx.try_recv().is_err());
        app.state.toggle_oversee(pane);
        app.advance_overseer(now + Duration::from_secs(41));
        assert!(submitted(&mut rx).await.contains("role has ended"));
        assert!(app.overseer_service.events_empty());
    }

    #[tokio::test]
    async fn overseer_transient_done_and_blocked_never_submit() {
        for transient in [AgentState::Idle, AgentState::Blocked] {
            let (mut app, pane, _, mut rx) = setup();
            let other = TerminalId::alloc();
            let mut t = TerminalState::new(other.clone(), std::env::temp_dir());
            t.state = AgentState::Working;
            app.state.terminals.insert(other.clone(), t);
            let now = Instant::now();
            app.state.toggle_oversee(pane);
            app.advance_overseer(now);
            submitted(&mut rx).await;
            app.state.terminals.get_mut(&other).unwrap().state = transient;
            app.advance_overseer(now + Duration::from_secs(1));
            app.advance_overseer(now + Duration::from_secs(19));
            app.state.terminals.get_mut(&other).unwrap().state = AgentState::Working;
            app.advance_overseer(now + Duration::from_secs(20));
            app.advance_overseer(now + Duration::from_secs(60));
            assert!(rx.try_recv().is_err());
            assert!(app.overseer_service.events_empty());
        }
    }

    #[tokio::test]
    async fn overseer_settle_is_configurable_and_restarts_after_state_changes() {
        let (mut app, pane, _, mut rx) = setup();
        let other = TerminalId::alloc();
        let mut t = TerminalState::new(other.clone(), std::env::temp_dir());
        t.state = AgentState::Working;
        t.agent_name = Some("worker".into());
        app.state.terminals.insert(other.clone(), t);
        app.overseer_service.config.event_settle_seconds = 30;
        let now = Instant::now();
        app.state.toggle_oversee(pane);
        app.advance_overseer(now);
        submitted(&mut rx).await;
        app.state.terminals.get_mut(&other).unwrap().state = AgentState::Idle;
        app.advance_overseer(now + Duration::from_secs(1));
        app.advance_overseer(now + Duration::from_secs(30));
        assert!(rx.try_recv().is_err());
        app.state.terminals.get_mut(&other).unwrap().state = AgentState::Working;
        app.advance_overseer(now + Duration::from_secs(31));
        app.state.terminals.get_mut(&other).unwrap().state = AgentState::Idle;
        app.advance_overseer(now + Duration::from_secs(32));
        app.advance_overseer(now + Duration::from_secs(61));
        assert!(rx.try_recv().is_err());
        app.advance_overseer(now + Duration::from_secs(62));
        assert!(submitted(&mut rx).await.contains("worker done"));
        app.advance_overseer(now + Duration::from_secs(100));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn overseer_revalidates_settled_events_while_holder_busy_and_on_removal() {
        let (mut app, pane, id, mut rx) = setup();
        let other = TerminalId::alloc();
        let mut t = TerminalState::new(other.clone(), std::env::temp_dir());
        t.state = AgentState::Working;
        app.state.terminals.insert(other.clone(), t);
        let now = Instant::now();
        app.state.toggle_oversee(pane);
        app.advance_overseer(now);
        submitted(&mut rx).await;
        app.state.terminals.get_mut(&id).unwrap().state = AgentState::Working;
        let t = app.state.terminals.get_mut(&other).unwrap();
        t.state = AgentState::Blocked;
        t.blocked_reason = Some("usage limit");
        app.advance_overseer(now + Duration::from_secs(1));
        app.advance_overseer(now + Duration::from_secs(21));
        assert!(rx.try_recv().is_err());
        app.state.terminals.get_mut(&other).unwrap().blocked_reason = Some("approval");
        app.state.terminals.get_mut(&id).unwrap().state = AgentState::Idle;
        app.advance_overseer(now + Duration::from_secs(22));
        app.advance_overseer(now + Duration::from_secs(41));
        assert!(rx.try_recv().is_err());
        app.advance_overseer(now + Duration::from_secs(42));
        let event = submitted(&mut rx).await;
        assert!(event.contains("blocked: approval"));
        assert!(!event.contains("usage limit"));
        app.state.terminals.get_mut(&other).unwrap().blocked_reason = Some("usage limit");
        app.advance_overseer(now + Duration::from_secs(43));
        app.state.terminals.remove(&other);
        app.advance_overseer(now + Duration::from_secs(80));
        assert!(rx.try_recv().is_err());
        assert!(app.overseer_service.events_empty());
    }

    #[tokio::test]
    async fn overseer_situation_uses_detected_model_then_pin_for_every_agent_kind() {
        let (mut app, pane, id, _) = setup();
        for agent in [
            Agent::Pi,
            Agent::Claude,
            Agent::Codex,
            Agent::Gemini,
            Agent::Cursor,
            Agent::Antigravity,
            Agent::Cline,
            Agent::OpenCode,
            Agent::GithubCopilot,
            Agent::Kimi,
            Agent::Kiro,
            Agent::Droid,
            Agent::Amp,
            Agent::Grok,
            Agent::Hermes,
            Agent::Kilo,
            Agent::Qodercli,
        ] {
            let t = app.state.terminals.get_mut(&id).unwrap();
            t.detected_agent = Some(agent);
            t.model_info = None;
            t.model_pin = crate::agent_model::ModelPin::new("pinned-model", None);
            let mut text = String::new();
            app.append_situation_pane(&mut text, pane, &id);
            assert!(text.contains("model: pinned-model;"), "{agent:?}: {text}");
            app.state.terminals.get_mut(&id).unwrap().model_info =
                Some(crate::agent_model::AgentModelInfo {
                    model: "detected-model".into(),
                    effort: None,
                });
            text.clear();
            app.append_situation_pane(&mut text, pane, &id);
            assert!(text.contains("model: detected-model;"), "{agent:?}: {text}");
            let t = app.state.terminals.get_mut(&id).unwrap();
            t.model_pin = None;
            t.model_info = None;
            text.clear();
            app.append_situation_pane(&mut text, pane, &id);
            assert!(text.contains("model: unknown;"));
        }
        let config: crate::overseer::OverseerConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(config.event_settle_seconds, 20);
    }

    #[tokio::test]
    async fn overseer_snapshot_and_live_handoff_preserve_role_without_duplicate_note() {
        use std::os::fd::IntoRawFd;
        use std::os::unix::net::UnixStream;
        let (mut app, pane, id, _rx) = setup();
        app.state.workspaces[0].overseer_scope = Some(Scope::Workspace);
        app.state.toggle_oversee(pane);
        assert_eq!(app.state.terminals[&id].overseer.kind(), Some(Kind::Space));
        app.state
            .terminals
            .get_mut(&id)
            .unwrap()
            .overseer
            .notices
            .clear();
        app.terminal_runtimes
            .get(&id)
            .unwrap()
            .record_input_bytes(b"draft with nested prompt marker > ");
        let snapshot = crate::persist::capture(&app.state, &app.terminal_runtimes);
        let saved = serde_json::to_string(&snapshot).unwrap();
        let snapshot: crate::persist::SessionSnapshot = serde_json::from_str(&saved).unwrap();
        assert!(saved.contains("\"active\":true"));
        let (master, _peer) = UnixStream::pair().unwrap();
        let mut imports = std::collections::HashMap::from([(
            pane.raw(),
            crate::handoff_runtime::ImportedHandoffRuntime {
                master_fd: master.into_raw_fd(),
                state: crate::handoff_runtime::HandoffRuntimeState {
                    pane_id: pane.raw(),
                    child_pid: 0,
                    rows: 24,
                    cols: 80,
                    cell_width_px: 0,
                    cell_height_px: 0,
                    keyboard_protocol_flags: 0,
                    keyboard_protocol_ansi: None,
                    input_state: None,
                    initial_history_ansi: None,
                },
            },
        )]);
        let (_, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let restored = App::new_from_handoff(
            &crate::config::Config::default(),
            None,
            api_rx,
            crate::api::EventHub::default(),
            &snapshot,
            &mut imports,
        )
        .unwrap();
        assert!(imports.is_empty());
        assert!(restored.state.terminals[&id].overseer.active);
        assert_eq!(
            restored.state.terminals[&id].overseer.kind(),
            Some(Kind::Space)
        );
        assert_eq!(
            restored.state.workspaces[0].overseer_scope,
            Some(Scope::Workspace)
        );
        assert!(restored.state.terminals[&id].overseer.notices.is_empty());
        assert!(!restored
            .terminal_runtimes
            .get(&id)
            .unwrap()
            .input_is_quiet(Instant::now() + Duration::from_secs(3)));
    }

    #[tokio::test]
    async fn overseer_profile_refuses_before_quit_without_resume_identity() {
        let (mut app, pane, _id, mut rx) = setup();
        app.no_session = false;
        app.overseer_service.config.codex_args =
            Some(vec!["--sandbox".into(), "workspace-write".into()]);
        app.state.toggle_oversee(pane);
        app.advance_overseer(Instant::now());
        let note = submitted(&mut rx).await;
        assert!(note.contains("agent is still running"));
        assert!(!note.contains("/quit"));
    }
    #[tokio::test]
    async fn overseer_no_session_never_quits_for_profile() {
        let (mut app, pane, id, mut rx) = setup();
        assert!(app.no_session);
        let terminal = app.state.terminals.get_mut(&id).unwrap();
        terminal.model_pin = crate::agent_model::ModelPin::new("test-model", None)
            .unwrap()
            .with_launch(Some("/bin/echo"), &[]);
        terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
            source: "herdr:codex".into(),
            agent: "codex".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id("same-conversation").unwrap(),
        });
        app.overseer_service.config.codex_args =
            Some(vec!["--sandbox".into(), "workspace-write".into()]);
        let now = Instant::now();
        let refusal = "permission profile needs the Herdr server; it was not applied";
        for active in [true, false] {
            // Also reject restoring a retained profile from an earlier server session.
            if !active {
                app.state
                    .terminals
                    .get_mut(&id)
                    .unwrap()
                    .overseer
                    .profile_base = Some(vec![
                    "/bin/echo".into(),
                    "resume".into(),
                    "same-conversation".into(),
                    "--model".into(),
                    "test-model".into(),
                ]);
            }
            app.state.toggle_oversee(pane);
            let tick = now + Duration::from_secs(if active { 0 } else { 3 });
            app.advance_overseer(tick);
            let note = submitted(&mut rx).await;
            assert!(note.contains(refusal), "{note}");
            assert!(note.contains(if active {
                "You are the overseer"
            } else {
                "role has ended"
            }));
            assert!(!note.contains("/quit"));
            assert!(!note.contains("/exit"));
            assert!(app.state.terminals[&id].pending_restart.is_none());
            assert!(app.state.terminals[&id].overseer.restart_argv.is_none());
            assert!(app.overseer_service.profile_probes.is_empty());
            assert!(app.state.terminals[&id].overseer.notices.is_empty());
            app.advance_overseer(tick + Duration::from_secs(1));
            assert!(rx.try_recv().is_err());
        }
        assert!(app.overseer_service.situation_path.is_file());
    }

    #[tokio::test]
    async fn overseer_profile_relaunches_same_conversation_model_and_removes_profile() {
        use std::os::unix::fs::PermissionsExt;
        let (mut app, pane, id, mut rx) = setup();
        app.no_session = false;
        let root = app
            .overseer_service
            .situation_path
            .parent()
            .unwrap()
            .to_path_buf();
        std::fs::create_dir_all(&root).unwrap();
        let launcher = root.join("codex");
        std::fs::write(&launcher, "#!/bin/sh\nfor arg in \"$@\"; do if [ \"$arg\" = --help ]; then exit 0; fi; done\nprintf '%s\\n' \"$@\" >> \"$0.args\"\nprintf 'ready\\n'\n").unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
        let terminal = app.state.terminals.get_mut(&id).unwrap();
        terminal.model_pin = crate::agent_model::ModelPin::new("test-model", None)
            .unwrap()
            .with_launch(launcher.to_str(), &[]);
        terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
            source: "herdr:codex".into(),
            agent: "codex".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id("same-conversation").unwrap(),
        });
        app.overseer_service.config.codex_args =
            Some(vec!["--sandbox".into(), "workspace-write".into()]);
        let now = Instant::now();
        // Exercise the post-probe guard with each readiness condition lost.
        // Calling the profile step directly models a successful earlier check.
        app.state.terminals.get_mut(&id).unwrap().state = AgentState::Working;
        assert_eq!(app.overseer_profile(&id, true, now), Ok(None));
        app.state.terminals.get_mut(&id).unwrap().state = AgentState::Idle;
        app.terminal_runtimes
            .get(&id)
            .unwrap()
            .record_input_bytes(b"draft");
        assert_eq!(app.overseer_profile(&id, true, now), Ok(None));
        app.terminal_runtimes
            .get(&id)
            .unwrap()
            .record_input_bytes(b"\x7f");
        assert_eq!(app.overseer_profile(&id, true, now), Ok(None));
        app.terminal_runtimes
            .get(&id)
            .unwrap()
            .test_process_pty_bytes(b"\x1b[2J\x1b[Hdone\r\n\xe2\x80\xba draft\r\n");
        assert_eq!(
            app.overseer_profile(&id, true, now + Duration::from_secs(3)),
            Ok(None)
        );
        assert!(rx.try_recv().is_err());
        assert!(app.state.terminals[&id].pending_restart.is_none());
        assert!(app.state.terminals[&id].overseer.profile_base.is_none());
        clear(&app, &id);
        app.state.toggle_oversee(pane);
        assert!(!app.state.terminals[&id].overseer.notices[0].profile_checked);
        let now = now + Duration::from_secs(3);
        app.advance_overseer(now);
        assert!(rx.try_recv().is_err());
        assert!(!app.state.terminals[&id].overseer.notices[0].profile_checked);
        let now = now + Duration::from_secs(1);
        app.advance_overseer(now);
        assert_eq!(submitted(&mut rx).await, "/quit\r");
        app.advance_agent_restarts(now + Duration::from_secs(1));
        app.advance_agent_restarts(now + Duration::from_secs(2));
        let command = submitted(&mut rx).await;
        assert!(command.contains("same-conversation"));
        assert!(command.contains("test-model"));
        assert!(command.contains("workspace-write"));
        assert!(std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command.trim())
            .status()
            .unwrap()
            .success());
        let args = std::fs::read_to_string(root.join("codex.args")).unwrap();
        assert!(args.contains("same-conversation"));
        assert!(args.contains("workspace-write"));
        let t = app.state.terminals.get_mut(&id).unwrap();
        t.detected_agent = Some(Agent::Codex);
        t.state = AgentState::Idle;
        clear(&app, &id);
        app.advance_overseer(now + Duration::from_secs(4));
        assert!(submitted(&mut rx).await.contains("You are the overseer"));
        app.state.toggle_oversee(pane);
        app.advance_overseer(now + Duration::from_secs(5));
        assert!(rx.try_recv().is_err());
        app.advance_overseer(now + Duration::from_secs(6));
        assert_eq!(submitted(&mut rx).await, "/quit\r");
        app.advance_agent_restarts(now + Duration::from_secs(7));
        app.advance_agent_restarts(now + Duration::from_secs(8));
        let command = submitted(&mut rx).await;
        assert!(command.contains("same-conversation"));
        assert!(command.contains("test-model"));
        assert!(!command.contains("workspace-write"));
        assert!(std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command.trim())
            .status()
            .unwrap()
            .success());
        assert!(app.state.terminals[&id].overseer.profile_base.is_none());
    }
    #[tokio::test]
    async fn native_turn_reporting_batches_distinct_end_outcomes_once() {
        use crate::turn_reporting::TurnEvent;
        for agent in [Agent::Claude, Agent::Codex, Agent::Cursor] {
            let (mut app, pane, _, mut rx) = setup();
            let other = TerminalId::alloc();
            let mut t = TerminalState::new(other.clone(), std::env::temp_dir());
            t.detected_agent = Some(agent);
            t.agent_name = Some("worker".into());
            t.job_marker = Some(
                app.overseer_service
                    .situation_path
                    .with_file_name("missing.done"),
            );
            t.report_turn(agent, TurnEvent::Started, 1, None);
            app.state.terminals.insert(other.clone(), t);
            let now = Instant::now();
            app.state.toggle_oversee(pane);
            app.advance_overseer(now);
            submitted(&mut rx).await;
            app.state.terminals.get_mut(&other).unwrap().report_turn(
                agent,
                TurnEvent::Ended,
                2,
                None,
            );
            app.advance_overseer(now + Duration::from_secs(1));
            assert!(rx.try_recv().is_err());
            app.advance_overseer(now + Duration::from_secs(121));
            assert!(rx.try_recv().is_err());
            app.advance_overseer(now + Duration::from_secs(141));
            assert!(submitted(&mut rx)
                .await
                .contains("worker stopped without a hand-off"));
            app.advance_overseer(now + Duration::from_secs(142));
            assert!(rx.try_recv().is_err());
            let folder = std::env::temp_dir().join(format!("turn-feed-{other}"));
            std::fs::create_dir_all(&folder).unwrap();
            let marker = folder.join("job.done");
            std::fs::write(&marker, "HANDOFF\ndone: complete result with verified evidence\ntests: focused test passed\n").unwrap();
            let t = app.state.terminals.get_mut(&other).unwrap();
            t.job_marker = Some(marker);
            t.report_turn(agent, TurnEvent::Started, 3, None);
            app.collect_overseer_events(now + Duration::from_secs(143));
            app.state.terminals.get_mut(&other).unwrap().report_turn(
                agent,
                TurnEvent::Ended,
                4,
                None,
            );
            app.advance_overseer(now + Duration::from_secs(144));
            app.advance_overseer(now + Duration::from_secs(164));
            assert!(submitted(&mut rx).await.contains("worker finished"));
        }
    }

    #[tokio::test]
    async fn task_stops_require_assignment_and_recheck_the_handoff() {
        use crate::turn_reporting::TurnEvent;
        // All native reporters: short reply, absent marker, empty marker,
        // completed marker, late hand-off, and dispatcher-owned stop.
        for agent in [Agent::Claude, Agent::Codex, Agent::Cursor] {
            for case in ["reply", "absent", "empty", "complete", "late", "dispatch"] {
                let (mut app, pane, _, mut rx) = setup();
                let other = TerminalId::alloc();
                let mut worker = TerminalState::new(other.clone(), std::env::temp_dir());
                worker.detected_agent = Some(agent);
                // A pane name must not be mistaken for its outcome text.
                worker.agent_name = Some(if case == "late" { "finished" } else { "maple" }.into());
                let folder = app
                    .overseer_service
                    .situation_path
                    .parent()
                    .unwrap()
                    .to_path_buf();
                std::fs::create_dir_all(&folder).unwrap();
                let marker = folder.join("stage.done");
                if case != "reply" {
                    worker.job_marker = Some(marker.clone());
                }
                if case == "empty" {
                    std::fs::write(&marker, "still writing").unwrap();
                }
                if case == "complete" {
                    std::fs::write(&marker, "HANDOFF\ndone: complete result with verified evidence\ntests: focused test passed\n").unwrap();
                }
                if case == "dispatch" {
                    std::fs::write(
                        &app.overseer_service.queue_path,
                        format!(
                            "stage\tmaple\tbrief\t-\t{}\tbuild\t-\t10\tsent\n",
                            marker.display()
                        ),
                    )
                    .unwrap();
                }
                worker.report_turn(agent, TurnEvent::Started, 1, None);
                app.state.terminals.insert(other.clone(), worker);
                let now = Instant::now();
                app.state.toggle_oversee(pane);
                app.advance_overseer(now);
                submitted(&mut rx).await;
                app.state.terminals.get_mut(&other).unwrap().report_turn(
                    agent,
                    TurnEvent::Ended,
                    2,
                    None,
                );
                app.advance_overseer(now + Duration::from_secs(1));
                if case == "late" {
                    std::fs::write(&marker, "HANDOFF\ndone: complete result with verified evidence\ntests: focused test passed\n").unwrap();
                }
                app.advance_overseer(now + Duration::from_secs(22));
                if matches!(case, "absent" | "empty") {
                    assert!(rx.try_recv().is_err(), "{agent:?}/{case}: early stop");
                    app.advance_overseer(now + Duration::from_secs(121));
                    app.advance_overseer(now + Duration::from_secs(142));
                }
                if matches!(case, "absent" | "empty" | "complete") {
                    let line = submitted(&mut rx).await;
                    let expected = if case == "complete" {
                        "maple finished"
                    } else {
                        "maple stopped without a hand-off"
                    };
                    assert!(line.contains(expected), "{agent:?}/{case}: {line}");
                } else {
                    assert!(rx.try_recv().is_err(), "{agent:?}/{case} woke the overseer");
                }
                app.advance_overseer(now + Duration::from_secs(164));
                assert!(rx.try_recv().is_err(), "duplicate {case}");
            }
        }
    }

    async fn short_reply_after_direct_stage(completed: bool) {
        use crate::turn_reporting::TurnEvent;
        for agent in [Agent::Claude, Agent::Codex, Agent::Cursor] {
            let (mut app, pane, _, mut rx) = setup();
            let other = TerminalId::alloc();
            let mut worker = TerminalState::new(other.clone(), std::env::temp_dir());
            worker.detected_agent = Some(agent);
            worker.agent_name = Some("maple".into());
            let folder = app.overseer_service.situation_path.parent().unwrap();
            std::fs::create_dir_all(folder).unwrap();
            let marker = folder.join("stage.done");
            if completed {
                std::fs::write(&marker, "HANDOFF\ndone: complete result with verified evidence\ntests: focused test passed\n").unwrap();
            }
            worker.job_marker = Some(marker.clone());
            worker.report_turn(agent, TurnEvent::Started, 1, None);
            app.state.terminals.insert(other.clone(), worker);
            let now = Instant::now();
            app.state.toggle_oversee(pane);
            app.advance_overseer(now);
            submitted(&mut rx).await;
            app.state.terminals.get_mut(&other).unwrap().report_turn(
                agent,
                TurnEvent::Ended,
                2,
                None,
            );
            app.advance_overseer(now + Duration::from_secs(1));
            assert_eq!(app.state.terminals[&other].job_marker.is_none(), completed);
            if !completed {
                assert!(app.overseer_service.events_empty());
                app.advance_overseer(now + Duration::from_secs(120));
                assert!(app.overseer_service.events_empty());
                assert!(rx.try_recv().is_err());
                app.advance_overseer(now + Duration::from_secs(121));
                // A stop queues once after silence, without consuming its marker.
                assert_eq!(
                    app.state.terminals[&other].job_marker.as_ref(),
                    Some(&marker)
                );
            }
            let delivered_at = if completed { 22 } else { 142 };
            app.advance_overseer(now + Duration::from_secs(delivered_at));
            let line = submitted(&mut rx).await;
            let expected = if completed {
                "maple finished"
            } else {
                "maple stopped without a hand-off"
            };
            assert!(line.contains(expected), "{agent:?}: {line}");
            // Replies after either delivered outcome never repeat that alarm.
            let worker = app.state.terminals.get_mut(&other).unwrap();
            worker.report_turn(agent, TurnEvent::Started, 3, None);
            worker.report_turn(agent, TurnEvent::Ended, 4, None);
            app.advance_overseer(now + Duration::from_secs(delivered_at + 1));
            app.advance_overseer(now + Duration::from_secs(delivered_at + 150));
            assert!(
                rx.try_recv().is_err(),
                "{agent:?}: short reply woke overseer"
            );
            if !completed {
                // A later real HANDOFF still completes the retained assignment.
                std::fs::write(&marker, "HANDOFF\ndone: recovered result with validated evidence\ntests: focused test passed\n").unwrap();
                let worker = app.state.terminals.get_mut(&other).unwrap();
                worker.report_turn(agent, TurnEvent::Started, 5, None);
                worker.report_turn(agent, TurnEvent::Ended, 6, None);
                app.advance_overseer(now + Duration::from_secs(delivered_at + 151));
                assert!(app.state.terminals[&other].job_marker.is_none());
                app.advance_overseer(now + Duration::from_secs(delivered_at + 172));
                assert!(submitted(&mut rx).await.contains("maple finished"));
            }
        }
    }

    #[tokio::test]
    async fn short_reply_after_completed_direct_stage_does_not_wake() {
        short_reply_after_direct_stage(true).await;
    }

    #[tokio::test]
    async fn short_reply_after_stopped_direct_stage_does_not_wake() {
        short_reply_after_direct_stage(false).await;
    }

    #[tokio::test]
    async fn question_turn_keeps_assignment_until_final_handoff() {
        use crate::turn_reporting::TurnEvent;
        for agent in [Agent::Claude, Agent::Codex, Agent::Cursor] {
            let (mut app, pane, _, mut rx) = setup();
            let other = TerminalId::alloc();
            let mut worker = TerminalState::new(other.clone(), std::env::temp_dir());
            worker.detected_agent = Some(agent);
            worker.agent_name = Some("maple".into());
            let folder = app.overseer_service.situation_path.parent().unwrap();
            std::fs::create_dir_all(folder).unwrap();
            let marker = folder.join("question-stage.done");
            worker.job_marker = Some(marker.clone());
            worker.report_turn(agent, TurnEvent::Started, 1, None);
            app.state.terminals.insert(other.clone(), worker);
            let now = Instant::now();
            app.state.toggle_oversee(pane);
            app.advance_overseer(now);
            submitted(&mut rx).await;
            app.state.terminals.get_mut(&other).unwrap().report_turn(
                agent,
                TurnEvent::Ended,
                2,
                None,
            );
            // A plain question uses the real native state, with no invented blocker.
            assert_eq!(app.state.terminals[&other].state, AgentState::Idle);
            assert!(app.state.terminals[&other].blocked_reason.is_none());
            app.advance_overseer(now + Duration::from_secs(1));
            app.advance_overseer(now + Duration::from_secs(60));
            assert!(app.overseer_service.events_empty());
            assert!(rx.try_recv().is_err());
            assert_eq!(
                app.state.terminals[&other].job_marker.as_ref(),
                Some(&marker)
            );
            app.state.terminals.get_mut(&other).unwrap().report_turn(
                agent,
                TurnEvent::Started,
                3,
                None,
            );
            app.advance_overseer(now + Duration::from_secs(61));
            // Cross the question's old deadline while the answer is being handled.
            app.advance_overseer(now + Duration::from_secs(130));
            assert!(app.overseer_service.events_empty());
            assert!(rx.try_recv().is_err());
            std::fs::write(&marker, "HANDOFF\ndone: complete result with verified evidence\ntests: focused test passed\n").unwrap();
            app.state.terminals.get_mut(&other).unwrap().report_turn(
                agent,
                TurnEvent::Ended,
                4,
                None,
            );
            app.advance_overseer(now + Duration::from_secs(131));
            assert!(app.state.terminals[&other].job_marker.is_none());
            app.advance_overseer(now + Duration::from_secs(152));
            let line = submitted(&mut rx).await;
            assert!(line.contains("maple finished"), "{agent:?}: {line}");
            assert!(!line.contains("stopped"), "{agent:?}: {line}");
            let worker = app.state.terminals.get_mut(&other).unwrap();
            worker.report_turn(agent, TurnEvent::Started, 5, None);
            worker.report_turn(agent, TurnEvent::Ended, 6, None);
            app.advance_overseer(now + Duration::from_secs(153));
            app.advance_overseer(now + Duration::from_secs(300));
            assert!(
                rx.try_recv().is_err(),
                "{agent:?}: duplicate outcome after reply"
            );
        }
    }

    #[tokio::test]
    async fn direct_stop_timer_restarts_for_between_tick_answer_and_new_marker() {
        use crate::turn_reporting::TurnEvent;
        for agent in [Agent::Claude, Agent::Codex, Agent::Cursor] {
            let (mut app, pane, _, mut rx) = setup();
            let other = TerminalId::alloc();
            let mut worker = TerminalState::new(other.clone(), std::env::temp_dir());
            worker.detected_agent = Some(agent);
            worker.agent_name = Some("maple".into());
            let marker = app
                .overseer_service
                .situation_path
                .with_file_name("one.done");
            worker.job_marker = Some(marker.clone());
            worker.report_turn(agent, TurnEvent::Started, 1, None);
            app.state.terminals.insert(other.clone(), worker);
            let now = Instant::now();
            app.state.toggle_oversee(pane);
            app.advance_overseer(now);
            submitted(&mut rx).await;
            app.state.terminals.get_mut(&other).unwrap().report_turn(
                agent,
                TurnEvent::Ended,
                2,
                None,
            );
            app.advance_overseer(now + Duration::from_secs(1));
            // Both native boundaries arrive before the next collector pass.
            let worker = app.state.terminals.get_mut(&other).unwrap();
            worker.report_turn(agent, TurnEvent::Started, 3, None);
            worker.report_turn(agent, TurnEvent::Ended, 4, None);
            app.advance_overseer(now + Duration::from_secs(100));
            app.advance_overseer(now + Duration::from_secs(121));
            assert!(app.overseer_service.events_empty());
            assert!(rx.try_recv().is_err());
            app.advance_overseer(now + Duration::from_secs(220));
            app.advance_overseer(now + Duration::from_secs(241));
            assert!(submitted(&mut rx)
                .await
                .contains("maple stopped without a hand-off"));
            // A different marker starts a new stage, with a fresh dedupe state.
            let worker = app.state.terminals.get_mut(&other).unwrap();
            worker.job_marker = Some(marker.with_file_name("two.done"));
            worker.report_turn(agent, TurnEvent::Started, 5, None);
            worker.report_turn(agent, TurnEvent::Ended, 6, None);
            app.advance_overseer(now + Duration::from_secs(242));
            app.advance_overseer(now + Duration::from_secs(362));
            app.advance_overseer(now + Duration::from_secs(383));
            assert!(submitted(&mut rx)
                .await
                .contains("maple stopped without a hand-off"));
            app.advance_overseer(now + Duration::from_secs(550));
            assert!(rx.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn information_stays_in_situation_and_real_escalations_wake() {
        let (mut app, panes, mut bus, now) = staffed().await;
        let caller = format!("p_{}", panes["alder"].0.raw());
        // Exercise the API's wire default: only explicit false is quiet.
        for (index, (text, extra, wake)) in [
            ("test is running; report follows", ",\"wake\":false", false),
            ("export needs a decision", ",\"wake\":true", true),
            ("export failed", "", true),
            ("unclassified line", "", true),
        ]
        .into_iter()
        .enumerate()
        {
            let at = now + Duration::from_secs(22 * (index as u64 + 1));
            let params = serde_json::from_str(&format!(
                "{{\"caller_pane_id\":\"{caller}\",\"text\":\"{text}\"{extra}}}"
            ))
            .unwrap();
            let response: serde_json::Value =
                serde_json::from_str(&app.handle_api_request(crate::api::schema::Request {
                    id: "line".into(),
                    method: crate::api::schema::Method::OverseerEscalate(params),
                }))
                .unwrap();
            assert_eq!(response["result"]["type"], "role_message_receipt");
            assert_eq!(
                response["result"]["state"],
                if wake { "queued" } else { "deferred" }
            );
            assert_eq!(
                response["result"]["message_ids"].as_array().unwrap().len(),
                1
            );
            app.advance_overseer(at);
            let receiver = bus.get_mut("cedar").unwrap();
            if wake {
                assert!(submitted(receiver).await.contains(text));
            } else {
                assert!(receiver.try_recv().is_err());
            }
            let situation = std::fs::read_to_string(
                app.overseer_situation_path(&panes["cedar"].1, Kind::Chief),
            )
            .unwrap();
            assert!(situation.contains(text), "{situation}");
        }
    }

    #[tokio::test]
    async fn queue_ownership_is_only_active_stages_with_markers() {
        let (app, _, _, _) = setup();
        let queue = &app.overseer_service.queue_path;
        std::fs::create_dir_all(queue.parent().unwrap()).unwrap();
        std::fs::write(
            queue,
            concat!(
                "# header\n",
                "a\tmaple\tbrief\t-\t/tmp/a.done\tbuild\t-\t10\tsent\n",
                "b\tfern\tbrief\t-\t/tmp/b.done\treview\t-\t10\tescalated\n",
                "c\treed\tbrief\t-\t-\tbuild\t-\t10\tsent\n",
                "d\tmoss\tbrief\t-\t/tmp/d.done\tchore\t-\t10\tsent\n",
                "e\tkelp\tbrief\t-\t/tmp/e.done\tbuild\t-\t10\tdone\n",
                "f\tbriar\tbrief\t-\t/tmp/f.done\tbuild\t-\t10\twaiting\n",
            ),
        )
        .unwrap();
        let owned = crate::overseer::dispatched_stages(queue);
        assert_eq!(owned.len(), 2);
        assert!(owned.contains_key("maple"));
        assert!(owned.contains_key("fern"));
        assert!(crate::overseer::dispatched_stages(&queue.with_extension("absent")).is_empty());
    }

    type Bus = HashMap<&'static str, tokio::sync::mpsc::Receiver<bytes::Bytes>>;
    type Panes = HashMap<&'static str, (PaneId, TerminalId)>;

    /// Three spaces with made up agents. Resolve space: alder, birch. Team
    /// space: cedar, dune. Docs space: ember.
    fn world() -> (App, Panes, Bus) {
        let (mut app, _, _, _) = setup();
        app.state = AppState::test_new();
        app.terminal_runtimes = Default::default();
        let mut spaces = Vec::new();
        for (space, names) in [
            ("Resolve space", vec!["alder", "birch"]),
            ("Team space", vec!["cedar", "dune"]),
            ("Docs space", vec!["ember"]),
        ] {
            let mut ws = crate::workspace::Workspace::test_new(space);
            let mut panes = vec![ws.tabs[0].root_pane];
            for _ in 1..names.len() {
                panes.push(ws.test_split(ratatui::layout::Direction::Horizontal));
            }
            spaces.push((ws, names, panes));
        }
        let mut panes_by_name = Panes::new();
        let mut bus = Bus::new();
        for (ws, names, panes) in spaces {
            for (name, pane) in names.into_iter().zip(panes) {
                let id = ws.terminal_id(pane).unwrap().clone();
                let mut t = TerminalState::new(id.clone(), std::env::temp_dir());
                t.detected_agent = Some(Agent::Codex);
                t.state = AgentState::Idle;
                t.agent_name = Some(name.into());
                let (rt, rx) = TerminalRuntime::test_with_channel(80, 24);
                rt.test_process_pty_bytes(b"checks passed\r\n\xe2\x80\xba \r\n");
                app.terminal_runtimes.insert(id.clone(), rt);
                app.state.terminals.insert(id.clone(), t);
                panes_by_name.insert(name, (pane, id));
                bus.insert(name, rx);
            }
            app.state.workspaces.push(ws);
        }
        app.state.active = Some(0);
        app.state.overseer_scope = Scope::Workspace;
        let folder = std::env::temp_dir().join(format!("herdr-overseer-{}", TerminalId::alloc()));
        app.overseer_service.situation_path = folder.join("situation.md");
        app.overseer_service.queue_path = folder.join("queue.tsv");
        (app, panes_by_name, bus)
    }

    fn kind_of(app: &App, panes: &Panes, name: &str) -> Option<Kind> {
        app.state.terminals[&panes[name].1].overseer.kind()
    }

    fn block(app: &mut App, panes: &Panes, name: &str, reason: &'static str) {
        let t = app.state.terminals.get_mut(&panes[name].1).unwrap();
        t.state = AgentState::Blocked;
        t.blocked_reason = Some(reason);
    }

    /// Alder is the overseer of Resolve space and cedar is the chief.
    async fn staffed() -> (App, Panes, Bus, Instant) {
        let (mut app, panes, mut bus) = world();
        for (name, (_, id)) in &panes {
            app.state
                .terminals
                .get_mut(id)
                .unwrap()
                .set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
                    source: "test".into(),
                    agent: "codex".into(),
                    session_ref: crate::agent_resume::AgentSessionRef::id(format!(
                        "{name}-session"
                    ))
                    .unwrap(),
                });
        }
        app.state.toggle_oversee(panes["alder"].0);
        app.state.toggle_oversee(panes["cedar"].0);
        app.state.toggle_oversee(panes["cedar"].0);
        assert_eq!(kind_of(&app, &panes, "alder"), Some(Kind::Space));
        assert_eq!(kind_of(&app, &panes, "cedar"), Some(Kind::Chief));
        let now = Instant::now();
        app.advance_overseer(now);
        assert!(submitted(bus.get_mut("alder").unwrap())
            .await
            .contains("You cover the space \"Resolve space\" only"));
        assert!(submitted(bus.get_mut("cedar").unwrap())
            .await
            .contains("chief overseer of every space"));
        (app, panes, bus, now)
    }

    #[tokio::test]
    async fn session_scope_has_one_overseer_and_it_is_the_chief() {
        let (mut app, panes, _bus) = world();
        app.state.overseer_scope = Scope::Session;
        app.state.toggle_oversee(panes["alder"].0);
        assert_eq!(kind_of(&app, &panes, "alder"), Some(Kind::Chief));
        app.state.toggle_oversee(panes["cedar"].0);
        assert_eq!(kind_of(&app, &panes, "alder"), None);
        assert_eq!(kind_of(&app, &panes, "cedar"), Some(Kind::Chief));
        app.state.toggle_oversee(panes["cedar"].0);
        assert_eq!(kind_of(&app, &panes, "cedar"), None);
    }

    #[tokio::test]
    async fn workspace_scope_steps_space_chief_off_with_one_per_space_and_one_chief() {
        let (mut app, panes, _bus) = world();
        let step = |app: &mut App, name: &str| app.state.toggle_oversee(panes[name].0);
        step(&mut app, "alder");
        assert_eq!(kind_of(&app, &panes, "alder"), Some(Kind::Space));
        step(&mut app, "alder");
        assert_eq!(kind_of(&app, &panes, "alder"), Some(Kind::Chief));
        step(&mut app, "alder");
        assert_eq!(kind_of(&app, &panes, "alder"), None);
        // One overseer per space: a second pane in the space takes it over.
        step(&mut app, "alder");
        step(&mut app, "birch");
        assert_eq!(kind_of(&app, &panes, "alder"), None);
        assert_eq!(kind_of(&app, &panes, "birch"), Some(Kind::Space));
        // Another space has its own.
        step(&mut app, "cedar");
        assert_eq!(kind_of(&app, &panes, "birch"), Some(Kind::Space));
        assert_eq!(kind_of(&app, &panes, "cedar"), Some(Kind::Space));
        // A chief leaves the space overseers alone, and stepping a space
        // overseer past a standing chief just switches it off.
        step(&mut app, "cedar");
        assert_eq!(kind_of(&app, &panes, "cedar"), Some(Kind::Chief));
        step(&mut app, "ember");
        assert_eq!(kind_of(&app, &panes, "ember"), Some(Kind::Space));
        step(&mut app, "ember");
        assert_eq!(kind_of(&app, &panes, "ember"), None);
        assert_eq!(kind_of(&app, &panes, "cedar"), Some(Kind::Chief));
        assert_eq!(kind_of(&app, &panes, "birch"), Some(Kind::Space));
        // One chief per session: with none standing, a space overseer can take the role.
        step(&mut app, "cedar");
        assert_eq!(kind_of(&app, &panes, "cedar"), None);
        step(&mut app, "alder");
        step(&mut app, "alder");
        assert_eq!(kind_of(&app, &panes, "alder"), Some(Kind::Chief));
        // The API reports the kind, and nothing for a pane without a role.
        let reported = |app: &App, ws: usize, name: &str| {
            serde_json::to_value(app.pane_info(ws, panes[name].0).unwrap()).unwrap()["overseer"]
                .clone()
        };
        assert_eq!(reported(&app, 0, "alder"), "chief");
        assert!(reported(&app, 0, "birch").is_null());
        step(&mut app, "dune");
        assert_eq!(reported(&app, 1, "dune"), "space");
        assert!(reported(&app, 2, "ember").is_null());
    }

    #[tokio::test]
    async fn a_space_can_override_the_setting_from_its_menu() {
        let (mut app, panes, _bus) = world();
        // Resolve space keeps one overseer for the session; the setting says otherwise.
        app.state.workspaces[0].overseer_scope = Some(Scope::Session);
        app.state.toggle_oversee(panes["alder"].0);
        assert_eq!(kind_of(&app, &panes, "alder"), Some(Kind::Chief));
        app.state.toggle_oversee(panes["cedar"].0);
        assert_eq!(kind_of(&app, &panes, "cedar"), Some(Kind::Space));
        assert_eq!(kind_of(&app, &panes, "alder"), Some(Kind::Chief));
        assert_eq!(
            app.state.cycle_space_overseer_scope(1),
            Some(Some(Scope::Workspace))
        );
        assert_eq!(
            app.state.cycle_space_overseer_scope(1),
            Some(Some(Scope::Session))
        );
        assert_eq!(app.state.cycle_space_overseer_scope(1), Some(None));
        assert_eq!(app.state.cycle_space_overseer_scope(9), None);
        let items = crate::app::state::ContextMenuState {
            kind: crate::app::state::ContextMenuKind::Workspace { ws_idx: 0 },
            x: 0,
            y: 0,
            list: crate::app::state::MenuListState::new(0),
        }
        .items();
        assert!(items.iter().any(|item| item == "Overseer scope"));
    }

    #[tokio::test]
    async fn each_overseer_hears_only_the_panes_it_covers() {
        let (mut app, panes, mut bus, now) = staffed().await;
        block(&mut app, &panes, "birch", "usage limit");
        block(&mut app, &panes, "dune", "rate limit");
        block(&mut app, &panes, "ember", "bad key");
        app.advance_overseer(now + Duration::from_secs(1));
        app.advance_overseer(now + Duration::from_secs(21));
        let space = submitted(bus.get_mut("alder").unwrap()).await;
        assert!(space.contains("birch blocked: usage limit"), "{space}");
        assert!(
            !space.contains("dune") && !space.contains("ember"),
            "{space}"
        );
        let chief = submitted(bus.get_mut("cedar").unwrap()).await;
        assert!(chief.contains("dune blocked: rate limit"), "{chief}");
        assert!(chief.contains("ember blocked: bad key"), "{chief}");
        assert!(chief.contains("Docs space: ember"), "{chief}");
        assert!(
            !chief.contains("birch"),
            "a space with an overseer stays with it: {chief}"
        );
        // The chief does hear when a space overseer itself is stopped.
        block(&mut app, &panes, "alder", "usage limit");
        app.advance_overseer(now + Duration::from_secs(22));
        app.advance_overseer(now + Duration::from_secs(60));
        let chief = submitted(bus.get_mut("cedar").unwrap()).await;
        assert!(chief.contains("alder blocked: usage limit"), "{chief}");
        assert!(bus.get_mut("alder").unwrap().try_recv().is_err());
    }

    #[tokio::test]
    async fn a_space_without_an_overseer_falls_to_the_chief_and_back() {
        let (mut app, panes, mut bus, now) = staffed().await;
        // Release the space overseer: Resolve space falls to the chief.
        app.state.toggle_oversee(panes["alder"].0);
        assert_eq!(kind_of(&app, &panes, "alder"), None);
        assert_eq!(kind_of(&app, &panes, "cedar"), Some(Kind::Chief));
        app.state
            .terminals
            .get_mut(&panes["alder"].1)
            .unwrap()
            .overseer
            .notices
            .clear();
        block(&mut app, &panes, "birch", "usage limit");
        app.advance_overseer(now + Duration::from_secs(1));
        app.advance_overseer(now + Duration::from_secs(22));
        let chief = submitted(bus.get_mut("cedar").unwrap()).await;
        assert!(chief.contains("birch blocked: usage limit"), "{chief}");
    }

    #[tokio::test]
    async fn escalation_goes_up_to_the_chief_only_and_messages_go_down_to_one_space_overseer() {
        let (mut app, panes, mut bus, now) = staffed().await;
        let refused = |result: Result<(), Refusal>| result.unwrap_err().0;
        let (alder, birch, cedar) = (panes["alder"].0, panes["birch"].0, panes["cedar"].0);
        // No other pane can use these.
        assert_eq!(
            app.overseer_escalate(birch, "x", true, now).unwrap_err().0,
            "not_allowed"
        );
        assert_eq!(
            app.overseer_escalate(cedar, "x", true, now).unwrap_err().0,
            "not_allowed"
        );
        assert_eq!(
            refused(app.overseer_message(birch, alder, "x", now)),
            "not_allowed"
        );
        assert_eq!(
            refused(app.overseer_message(alder, alder, "x", now)),
            "not_allowed"
        );
        // The chief reaches space overseers and nothing else.
        assert_eq!(
            refused(app.overseer_message(cedar, birch, "x", now)),
            "not_a_space_overseer"
        );
        assert_eq!(
            refused(app.overseer_message(cedar, cedar, "x", now)),
            "not_a_space_overseer"
        );
        // One plain line.
        assert_eq!(
            app.overseer_escalate(alder, "  ", true, now).unwrap_err().0,
            "empty_text"
        );
        assert_eq!(
            app.overseer_escalate(alder, "a\nb", true, now)
                .unwrap_err()
                .0,
            "one_line_only"
        );
        assert_eq!(
            app.overseer_escalate(alder, &"x".repeat(241), true, now)
                .unwrap_err()
                .0,
            "too_long"
        );
        assert_eq!(
            app.overseer_escalate(alder, "the export needs a decision", true, now),
            Ok(1)
        );
        app.overseer_message(cedar, alder, "hold the export", now)
            .unwrap();
        app.advance_overseer(now + Duration::from_secs(21));
        let up = submitted(bus.get_mut("cedar").unwrap()).await;
        assert!(
            up.contains("alder (space Resolve space) escalates: the export needs a decision"),
            "{up}"
        );
        assert!(!up.contains("hold the export"), "{up}");
        let down = submitted(bus.get_mut("alder").unwrap()).await;
        assert!(down.contains("chief cedar says: hold the export"), "{down}");
        assert!(!down.contains("needs a decision"), "{down}");
        for other in ["birch", "dune", "ember"] {
            assert!(
                bus.get_mut(other).unwrap().try_recv().is_err(),
                "{other} heard a private line"
            );
        }
        // Both lines are kept in the receiver's situation file.
        let chief_file =
            std::fs::read_to_string(app.overseer_situation_path(&panes["cedar"].1, Kind::Chief))
                .unwrap();
        assert!(
            chief_file.contains("Escalated by space overseers"),
            "{chief_file}"
        );
        assert!(
            chief_file.contains("the export needs a decision"),
            "{chief_file}"
        );
        let space_file =
            std::fs::read_to_string(app.overseer_situation_path(&panes["alder"].1, Kind::Space))
                .unwrap();
        assert!(
            space_file.contains("Messages from the chief"),
            "{space_file}"
        );
        assert!(space_file.contains("hold the export"), "{space_file}");
    }

    #[tokio::test]
    async fn escalation_needs_a_chief_and_a_full_mailbox_is_refused() {
        let (mut app, panes, _bus, now) = staffed().await;
        let alder = panes["alder"].0;
        for n in 0..crate::overseer::MAIL_LIMIT {
            app.overseer_escalate(alder, &format!("line {n}"), true, now)
                .unwrap();
        }
        assert_eq!(
            app.overseer_escalate(alder, "one more", true, now)
                .unwrap_err()
                .0,
            "mailbox_full"
        );
        // Without a chief nothing is sent anywhere.
        app.state.toggle_oversee(panes["cedar"].0);
        assert_eq!(kind_of(&app, &panes, "cedar"), None);
        assert_eq!(
            app.overseer_escalate(alder, "hello", true, now)
                .unwrap_err()
                .0,
            "no_chief"
        );
    }

    #[tokio::test]
    async fn pending_mail_survives_role_combinations_and_has_a_final_disposition() {
        let (mut app, panes, _bus, now) = staffed().await;
        let alder = panes["alder"].0;
        let cedar_id = panes["cedar"].1.clone();
        app.overseer_escalate(alder, "pending decision", true, now)
            .unwrap();
        let message_id = app
            .overseer_service
            .message_id(app.overseer_service.mail_seq);
        assert!(app.overseer_service.holders[&cedar_id]
            .events
            .contains_key(&message_id));

        // Add then release the second role. The shared terminal mailbox and
        // history remain attached to the still-held role in either direction.
        assert!(app.state.set_space_overseer(&cedar_id, true));
        app.sync_overseer_holders(now);
        assert!(app.overseer_service.holders[&cedar_id]
            .events
            .contains_key(&message_id));
        assert!(app.overseer_service.holders[&cedar_id]
            .mail
            .iter()
            .any(|line| line.contains(&message_id)));
        assert!(app.state.set_session_overlord(&cedar_id, false));
        app.sync_overseer_holders(now);
        assert!(app.overseer_service.holders[&cedar_id]
            .events
            .contains_key(&message_id));
        assert!(app.state.set_session_overlord(&cedar_id, true));
        app.sync_overseer_holders(now);
        assert!(app.overseer_service.holders[&cedar_id]
            .events
            .contains_key(&message_id));

        // Losing the addressed role cancels the queued item instead of leaving
        // a receipt that still claims queued delivery.
        app.state.set_space_overseer(&cedar_id, false);
        app.state.set_session_overlord(&cedar_id, false);
        app.sync_overseer_holders(now);
        let receipt = &app.overseer_service.message_deliveries[&message_id];
        assert_eq!(
            receipt.state,
            crate::overseer::MessageDeliveryState::Cancelled
        );
        assert_eq!(
            receipt.hold_reason.as_deref(),
            Some("receiver role was released")
        );
    }

    #[tokio::test]
    async fn pending_mail_revalidates_sender_receiver_and_workspace_bindings() {
        let (mut app, panes, _bus, now) = staffed().await;
        let alder_id = panes["alder"].1.clone();
        let cedar_id = panes["cedar"].1.clone();
        let original_space_id = app.state.workspaces[0].id.clone();
        for (id, session) in [(&alder_id, "alder-session"), (&cedar_id, "cedar-session")] {
            app.state
                .terminals
                .get_mut(id)
                .unwrap()
                .set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
                    source: "test".into(),
                    agent: "codex".into(),
                    session_ref: crate::agent_resume::AgentSessionRef::id(session).unwrap(),
                });
        }
        app.overseer_escalate(panes["alder"].0, "pending", true, now)
            .unwrap();
        let id = app
            .overseer_service
            .message_id(app.overseer_service.mail_seq);
        assert_eq!(app.mail_invalidation(&id), None);

        app.state
            .terminals
            .get_mut(&cedar_id)
            .unwrap()
            .set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
                source: "test".into(),
                agent: "codex".into(),
                session_ref: crate::agent_resume::AgentSessionRef::id("cedar-replaced").unwrap(),
            });
        assert!(app
            .mail_invalidation(&id)
            .unwrap()
            .contains("receiver native conversation changed"));

        app.state
            .terminals
            .get_mut(&alder_id)
            .unwrap()
            .set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
                source: "test".into(),
                agent: "codex".into(),
                session_ref: crate::agent_resume::AgentSessionRef::id("alder-session").unwrap(),
            });
        app.state.workspaces[0].id = "moved-space".into();
        assert!(app.mail_invalidation(&id).unwrap().contains("sender moved"));

        app.state.workspaces[0].id = original_space_id;
        app.state.set_space_overseer(&alder_id, false);
        assert!(app.mail_invalidation(&id).unwrap().contains("sender"));
    }

    #[tokio::test]
    async fn release_and_reacquire_invalidates_only_the_addressed_authority_before_a_batch() {
        for (downward, release_sender) in
            [(false, true), (false, false), (true, true), (true, false)]
        {
            let (mut app, panes, _bus, now) = staffed().await;
            let (sender, sender_id, receiver, receiver_id, role) = if downward {
                (
                    panes["cedar"].0,
                    panes["cedar"].1.clone(),
                    panes["alder"].0,
                    panes["alder"].1.clone(),
                    Kind::Chief,
                )
            } else {
                (
                    panes["alder"].0,
                    panes["alder"].1.clone(),
                    panes["cedar"].0,
                    panes["cedar"].1.clone(),
                    Kind::Space,
                )
            };
            for (id, session) in [
                (&sender_id, "sender-conversation"),
                (&receiver_id, "receiver-conversation"),
            ] {
                app.state
                    .terminals
                    .get_mut(id)
                    .unwrap()
                    .set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
                        source: "test".into(),
                        agent: "claude".into(),
                        session_ref: crate::agent_resume::AgentSessionRef::id(session).unwrap(),
                    });
            }
            if downward {
                app.overseer_message(sender, receiver, "same event", now)
                    .unwrap();
            } else {
                app.overseer_escalate(sender, "same event", true, now)
                    .unwrap();
            }
            let message_id = app
                .overseer_service
                .message_id(app.overseer_service.mail_seq);
            let (released_id, addressed_role) = if release_sender {
                (&sender_id, role)
            } else {
                (
                    &receiver_id,
                    if downward { Kind::Space } else { Kind::Chief },
                )
            };
            let conversation_before =
                app.state.terminals[released_id].current_session_identity_for_persistence();
            let release = |app: &mut App, id: &TerminalId, kind: Kind, enabled: bool| match kind {
                Kind::Space => app.state.set_space_overseer(id, enabled),
                Kind::Chief => app.state.set_session_overlord(id, enabled),
            };
            if addressed_role == Kind::Chief {
                assert!(app.state.set_space_overseer(released_id, true));
                assert_eq!(
                    app.mail_invalidation(&message_id),
                    None,
                    "changing only the other role must preserve the queued authorization"
                );
            }
            assert!(release(&mut app, released_id, addressed_role, false));
            assert!(release(&mut app, released_id, addressed_role, true));
            assert_eq!(
                app.state.terminals[released_id].current_session_identity_for_persistence(),
                conversation_before
            );
            assert!(app
                .mail_invalidation(&message_id)
                .is_some_and(|reason| reason.contains("role generation changed")));
        }
    }

    #[tokio::test]
    async fn logical_delivery_key_reuses_matching_receipt_and_refuses_binding_changes() {
        let (mut app, panes, _bus, now) = staffed().await;
        let sender_id = panes["cedar"].1.clone();
        let receiver_id = panes["alder"].1.clone();
        let receiver_pane = format!("p_{}", panes["alder"].0.raw());
        let other_receiver_pane = format!("p_{}", panes["dune"].0.raw());
        for (id, session) in [
            (&sender_id, "stable-sender-session"),
            (&receiver_id, "stable-receiver-session"),
        ] {
            app.state
                .terminals
                .get_mut(id)
                .unwrap()
                .set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
                    source: "test".into(),
                    agent: "claude".into(),
                    session_ref: crate::agent_resume::AgentSessionRef::id(session).unwrap(),
                });
        }
        let call =
            |request_id: &str, caller: String, to: &str, text: &str, delivery_key: Option<&str>| {
                crate::api::schema::Request {
                    id: request_id.into(),
                    method: crate::api::schema::Method::OverseerMessage(
                        crate::api::schema::OverseerMessageParams {
                            caller_pane_id: caller,
                            to: to.into(),
                            text: text.into(),
                            delivery_key: delivery_key.map(str::to_string),
                        },
                    ),
                }
            };
        let caller = format!("p_{}", panes["cedar"].0.raw());
        let first: serde_json::Value = serde_json::from_str(&app.handle_api_request(call(
            "first",
            caller.clone(),
            &receiver_pane,
            "same payload",
            Some("event-1"),
        )))
        .unwrap();
        let first_id = first["result"]["message_ids"][0]
            .as_str()
            .unwrap_or_else(|| panic!("first keyed delivery failed: {first}"))
            .to_string();
        let count_after_first = app.overseer_service.message_deliveries.len();

        let retry: serde_json::Value = serde_json::from_str(&app.handle_api_request(call(
            "retry",
            caller.clone(),
            &receiver_pane,
            "same payload",
            Some("event-1"),
        )))
        .unwrap();
        assert_eq!(retry["result"]["message_ids"][0], first_id);
        assert_eq!(
            app.overseer_service.message_deliveries.len(),
            count_after_first
        );

        let changed_payload: serde_json::Value =
            serde_json::from_str(&app.handle_api_request(call(
                "payload-change",
                caller.clone(),
                &receiver_pane,
                "different payload",
                Some("event-1"),
            )))
            .unwrap();
        assert_eq!(changed_payload["error"]["code"], "delivery_key_reused");

        let other_receiver = panes["dune"].1.clone();
        assert!(app.state.set_space_overseer(&other_receiver, true));
        app.sync_overseer_holders(now);
        let changed_receiver: serde_json::Value =
            serde_json::from_str(&app.handle_api_request(call(
                "receiver-change",
                caller.clone(),
                &other_receiver_pane,
                "same payload",
                Some("event-1"),
            )))
            .unwrap();
        assert_eq!(changed_receiver["error"]["code"], "delivery_key_reused");

        app.state
            .terminals
            .get_mut(&receiver_id)
            .unwrap()
            .set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
                source: "test".into(),
                agent: "claude".into(),
                session_ref: crate::agent_resume::AgentSessionRef::id("replaced-receiver-session")
                    .unwrap(),
            });
        let changed_conversation: serde_json::Value =
            serde_json::from_str(&app.handle_api_request(call(
                "conversation-change",
                caller.clone(),
                &receiver_pane,
                "same payload",
                Some("event-1"),
            )))
            .unwrap();
        assert_eq!(changed_conversation["error"]["code"], "delivery_key_reused");

        assert!(app.state.set_session_overlord(&sender_id, false));
        assert!(app.state.set_session_overlord(&sender_id, true));
        let changed_authority: serde_json::Value =
            serde_json::from_str(&app.handle_api_request(call(
                "authority-change",
                caller.clone(),
                &receiver_pane,
                "same payload",
                Some("event-1"),
            )))
            .unwrap();
        assert_eq!(changed_authority["error"]["code"], "delivery_key_reused");

        let deliberate_new: serde_json::Value = serde_json::from_str(&app.handle_api_request(
            call("new-event", caller, &receiver_pane, "same payload", None),
        ))
        .unwrap();
        assert_ne!(deliberate_new["result"]["message_ids"][0], first_id);
    }

    #[tokio::test]
    async fn logical_delivery_key_history_is_bounded() {
        let (mut app, panes, _bus, _now) = staffed().await;
        let caller = format!("p_{}", panes["alder"].0.raw());
        for index in 0..=crate::overseer::LOGICAL_DELIVERY_HISTORY {
            let params = crate::api::schema::OverseerEscalateParams {
                caller_pane_id: caller.clone(),
                text: format!("information {index}"),
                delivery_key: Some(format!("event-{index}")),
                wake: false,
            };
            let _: serde_json::Value =
                serde_json::from_str(&app.handle_api_request(crate::api::schema::Request {
                    id: "bounded-delivery".into(),
                    method: crate::api::schema::Method::OverseerEscalate(params),
                }))
                .unwrap();
        }
        assert_eq!(
            app.overseer_service.logical_deliveries.len(),
            crate::overseer::LOGICAL_DELIVERY_HISTORY
        );
        assert!(!app
            .overseer_service
            .logical_deliveries
            .contains_key("event-0"));
        assert!(app
            .overseer_service
            .logical_deliveries
            .contains_key("event-128"));
    }

    #[tokio::test]
    async fn role_memory_survives_space_overseer_takeover_and_status_exposes_its_path() {
        let (mut app, panes, _bus, _now) = staffed().await;
        let append = |app: &mut App, request_id: &str, terminal_pane: PaneId, text: &str| {
            app.handle_api_request(crate::api::schema::Request {
                id: request_id.into(),
                method: crate::api::schema::Method::RoleMemoryAppend(
                    crate::api::schema::RoleMemoryAppendParams {
                        caller_pane_id: format!("p_{}", terminal_pane.raw()),
                        role: crate::api::schema::ManagedRole::SpaceOverseer,
                        text: text.into(),
                    },
                ),
            })
        };
        let first: serde_json::Value = serde_json::from_str(&append(
            &mut app,
            "memory-old",
            panes["alder"].0,
            "old holder note",
        ))
        .unwrap();
        let first_path = first["result"]["path"].as_str().unwrap().to_string();
        assert_eq!(first["result"]["type"], "role_memory_appended");

        assert!(app.state.set_space_overseer(&panes["alder"].1, false));
        assert!(app.state.set_space_overseer(&panes["birch"].1, true));
        let second: serde_json::Value = serde_json::from_str(&append(
            &mut app,
            "memory-new",
            panes["birch"].0,
            "new holder note",
        ))
        .unwrap();
        assert_eq!(second["result"]["path"], first_path);
        let memory =
            crate::role_continuity::read_records(std::path::Path::new(&first_path)).unwrap();
        assert_eq!(memory.len(), 2);
        assert!(
            matches!(&memory[0], crate::role_continuity::ContinuityRecord::Memory { text, .. } if text == "old holder note")
        );
        assert!(
            matches!(&memory[1], crate::role_continuity::ContinuityRecord::Memory { text, .. } if text == "new holder note")
        );

        let status: serde_json::Value =
            serde_json::from_str(&app.handle_api_request(crate::api::schema::Request {
                id: "memory-status".into(),
                method: crate::api::schema::Method::RoleStatus(
                    crate::api::schema::RoleStatusParams {
                        caller_pane_id: format!("p_{}", panes["birch"].0.raw()),
                        inspected_pane_id: None,
                    },
                ),
            }))
            .unwrap();
        assert_eq!(
            status["result"]["status"]["working_memory"][0]["path"],
            first_path
        );
    }

    #[tokio::test]
    async fn both_roles_self_route_once_as_deferred_information_without_prompting() {
        let (mut app, panes, mut bus, now) = staffed().await;
        let cedar = panes["cedar"].0;
        let cedar_terminal = panes["cedar"].1.clone();
        assert!(app.state.set_space_overseer(&cedar_terminal, true));
        assert!(app.state.terminals[&cedar_terminal]
            .overseer
            .is_space_overseer());
        assert!(app.state.terminals[&cedar_terminal]
            .overseer
            .is_session_overlord());
        for terminal in app.state.terminals.values_mut() {
            terminal.overseer.notices.clear();
        }
        app.sync_overseer_holders(now);
        for holder in app.overseer_service.holders.values_mut() {
            holder.events.clear();
        }
        let receiver = bus.get_mut("cedar").unwrap();
        while receiver.try_recv().is_ok() {}

        let before = app.overseer_service.mail_seq;
        assert_eq!(
            app.overseer_escalate(cedar, "local status", true, now),
            Ok(1)
        );
        let upward_id = app.overseer_service.message_id(before + 1);
        assert_eq!(
            app.overseer_service.message_deliveries[&upward_id].state,
            crate::overseer::MessageDeliveryState::Deferred
        );
        assert!(!app.overseer_service.holders[&cedar_terminal]
            .events
            .contains_key(&upward_id));
        let saved_mail = &app.overseer_service.holders[&cedar_terminal].mail;
        assert_eq!(
            saved_mail
                .iter()
                .filter(|line| line.contains(&upward_id))
                .count(),
            1
        );
        assert!(saved_mail
            .iter()
            .any(|line| line.contains("self route; informational only")));

        app.overseer_message(cedar, cedar, "local reminder", now)
            .unwrap();
        let downward_id = app.overseer_service.message_id(before + 2);
        assert_eq!(
            app.overseer_service.message_deliveries[&downward_id].state,
            crate::overseer::MessageDeliveryState::Deferred
        );
        assert!(!app.overseer_service.holders[&cedar_terminal]
            .events
            .contains_key(&downward_id));
        assert_eq!(
            app.overseer_service.holders[&cedar_terminal]
                .mail
                .iter()
                .filter(|line| line.contains(&downward_id))
                .count(),
            1
        );

        app.advance_overseer(now + Duration::from_secs(21));
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn only_interface_input_changes_roles_and_escalation_changes_none() {
        let (mut app, panes, _bus, now) = staffed().await;
        for body in [
            r#"{"id":"1","method":"overseer.assign","params":{"pane_id":"p_1","kind":"chief"}}"#,
            r#"{"id":"2","method":"overseer.set","params":{"caller_pane_id":"p_1"}}"#,
            r#"{"id":"3","method":"overseer.escalate","params":{"text":"no caller"}}"#,
            r#"{"id":"4","method":"pane.set_overseer","params":{"overseer":"chief"}}"#,
        ] {
            assert!(
                serde_json::from_str::<crate::api::schema::Request>(body).is_err(),
                "{body}"
            );
        }
        let request: crate::api::schema::Request = serde_json::from_str(
            r#"{"id":"5","method":"overseer.message","params":{"caller_pane_id":"p_1","to":"alder","text":"hi"}}"#,
        )
        .unwrap();
        assert!(matches!(
            request.method,
            crate::api::schema::Method::OverseerMessage(_)
        ));
        app.overseer_escalate(panes["alder"].0, "hello", true, now)
            .unwrap();
        app.overseer_message(panes["cedar"].0, panes["alder"].0, "hi", now)
            .unwrap();
        assert_eq!(kind_of(&app, &panes, "alder"), Some(Kind::Space));
        assert_eq!(kind_of(&app, &panes, "cedar"), Some(Kind::Chief));
        assert_eq!(kind_of(&app, &panes, "birch"), None);
    }

    #[tokio::test]
    async fn a_released_overseers_file_says_it_has_ended() {
        let (mut app, panes, _bus, now) = staffed().await;
        app.advance_overseer(now + Duration::from_secs(2));
        let path = app.overseer_situation_path(&panes["alder"].1, Kind::Space);
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("Resolve space"));
        app.state.toggle_oversee(panes["alder"].0);
        app.advance_overseer(now + Duration::from_secs(4));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("has ended") && !text.contains("birch"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn situation_files_cover_their_own_scope_and_hold_no_input() {
        let (mut app, panes, _bus, _now) = staffed().await;
        let tasks = app
            .overseer_service
            .situation_path
            .with_file_name("tasks.json");
        std::fs::write(&tasks, r#"{"tasks":[{"title":"Chief only task"}]}"#).unwrap();
        app.overseer_service.tasks_path = Some(tasks);
        app.terminal_runtimes
            .get(&panes["birch"].1)
            .unwrap()
            .test_process_pty_bytes(b"\x1b[2J\x1b[Hbirch output\r\n\xe2\x80\xba SECRET_DRAFT\r\n");
        let later = Instant::now() + Duration::from_secs(30);
        app.advance_overseer(later);
        let space_path = app.overseer_situation_path(&panes["alder"].1, Kind::Space);
        let chief_path = app.overseer_situation_path(&panes["cedar"].1, Kind::Chief);
        assert_ne!(space_path, chief_path);
        assert!(space_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("space-"));
        let space = std::fs::read_to_string(space_path).unwrap();
        let chief = std::fs::read_to_string(chief_path).unwrap();
        assert!(
            space.contains("Resolve space") && space.contains("birch"),
            "{space}"
        );
        assert!(space.contains("birch output"), "{space}");
        for hidden in [
            "Team space",
            "Docs space",
            "dune",
            "ember",
            "Chief only task",
        ] {
            assert!(
                !space.contains(hidden),
                "{hidden} leaked into a space file: {space}"
            );
        }
        for shown in [
            "Resolve space",
            "Team space",
            "Docs space",
            "dune",
            "ember",
            "Chief only task",
        ] {
            assert!(
                chief.contains(shown),
                "{shown} missing from the chief file: {chief}"
            );
        }
        assert!(!space.contains("SECRET") && !chief.contains("SECRET"));
        assert!(
            chief.contains("roles: space overseer") && chief.contains("roles: session overlord"),
            "{chief}"
        );
    }

    #[tokio::test]
    async fn space_and_chief_roles_and_space_settings_survive_a_snapshot() {
        let (mut app, panes, _bus, _now) = staffed().await;
        app.state.workspaces[2].overseer_scope = Some(Scope::Session);
        for id in [&panes["alder"].1, &panes["cedar"].1] {
            app.state
                .terminals
                .get_mut(id)
                .unwrap()
                .overseer
                .notices
                .clear();
        }
        let snapshot = crate::persist::capture(&app.state, &app.terminal_runtimes);
        let saved = serde_json::to_string(&snapshot).unwrap();
        assert!(
            saved.contains("\"kind\":\"space\"") && saved.contains("\"kind\":\"chief\""),
            "{saved}"
        );
        assert!(saved.contains("\"overseer_scope\":\"session\""), "{saved}");
        let snapshot: crate::persist::SessionSnapshot = serde_json::from_str(&saved).unwrap();
        assert_eq!(
            snapshot
                .workspaces
                .iter()
                .filter(|w| w.overseer_scope.is_some())
                .count(),
            1
        );
        // A session saved before space overseers held its one overseer as the chief.
        let legacy: crate::overseer::Role = serde_json::from_str(r#"{"active":true}"#).unwrap();
        assert_eq!(legacy.kind(), Some(Kind::Chief));
        let migrated = legacy.restored(true);
        assert!(migrated.is_session_overlord());
        assert!(!migrated.is_space_overseer());
        let mut independent = crate::overseer::Role::default();
        assert!(independent.set_membership(Kind::Space, true));
        assert!(independent.set_membership(Kind::Chief, true));
        assert!(independent.is_space_overseer());
        assert!(independent.is_session_overlord());
        let revision = independent.revision;
        assert!(!independent.set_membership(Kind::Chief, true));
        assert_eq!(independent.revision, revision);
        assert!(independent.set_membership(Kind::Space, false));
        assert!(!independent.is_space_overseer());
        assert!(independent.is_session_overlord());
    }

    #[test]
    fn config_scope_defaults_to_session_and_rejects_anything_else() {
        let parse = |text: &str| toml::from_str::<crate::overseer::OverseerConfig>(text);
        assert_eq!(parse("").unwrap().scope, Scope::Session);
        assert_eq!(parse("scope = \"session\"").unwrap().scope, Scope::Session);
        assert_eq!(
            parse("scope = \"workspace\"").unwrap().scope,
            Scope::Workspace
        );
        assert!(parse("scope = \"galaxy\"").is_err());
    }

    #[test]
    fn changing_between_space_and_chief_queues_one_note_and_never_restarts_for_a_profile() {
        let mut role = crate::overseer::Role::default();
        role.assign(Some(Kind::Space));
        assert_eq!(role.notices.len(), 1);
        assert!(!role.notices[0].profile_checked);
        // Still waiting to deliver the first note: no second one.
        role.assign(Some(Kind::Chief));
        assert_eq!(role.notices.len(), 1);
        assert_eq!(role.kind(), Some(Kind::Chief));
        role.notices.clear();
        role.assign(Some(Kind::Space));
        assert_eq!(role.notices.len(), 1);
        assert!(
            role.notices[0].profile_checked,
            "a change of kind must not restart the agent"
        );
        role.notices.clear();
        role.assign(None);
        assert!(!role.notices[0].active);
        assert_eq!(role.kind(), None);
        role.assign(None);
        assert_eq!(role.notices.len(), 1);
    }

    // ---- R10 fix round 1: context transition caller and role card ----

    fn enable_transitions(app: &mut App) {
        app.overseer_service.config.context_transition_enabled = true;
        app.overseer_service.config.context_handoff_limit_percent = 80;
    }

    fn handoff_call(
        panes: &Panes,
        name: &str,
        role: crate::api::schema::ManagedRole,
        findings: Vec<String>,
    ) -> crate::api::schema::Request {
        crate::api::schema::Request {
            id: "handoff".into(),
            method: crate::api::schema::Method::RoleHandoffRecord(
                crate::api::schema::RoleHandoffRecordParams {
                    caller_pane_id: format!("p_{}", panes[name].0.raw()),
                    role,
                    task_id: "R10".into(),
                    attempt: "1".into(),
                    candidate: "abc".into(),
                    tests: vec!["cargo test".into()],
                    output_paths: vec!["R10-REPORT.md".into()],
                    open_findings: findings,
                    cost: "tracked".into(),
                    next_owner: "handoff owner".into(),
                },
            ),
        }
    }

    fn record_handoff(
        app: &mut App,
        panes: &Panes,
        name: &str,
        role: crate::api::schema::ManagedRole,
        findings: Vec<String>,
    ) -> serde_json::Value {
        serde_json::from_str(&app.handle_api_request(handoff_call(panes, name, role, findings)))
            .unwrap()
    }

    fn drain(rx: &mut tokio::sync::mpsc::Receiver<bytes::Bytes>) -> String {
        let mut out = String::new();
        while let Ok(bytes) = rx.try_recv() {
            out.push_str(&String::from_utf8_lossy(&bytes));
        }
        out
    }

    fn append_memory(app: &mut App, panes: &Panes, name: &str, role: ManagedRole, text: &str) {
        let reply = app.handle_api_request(crate::api::schema::Request {
            id: "memory".into(),
            method: crate::api::schema::Method::RoleMemoryAppend(
                crate::api::schema::RoleMemoryAppendParams {
                    caller_pane_id: format!("p_{}", panes[name].0.raw()),
                    role,
                    text: text.into(),
                },
            ),
        });
        assert!(reply.contains("role_memory_appended"), "{reply}");
    }

    use crate::api::schema::ManagedRole;

    /// Cedar is the session overlord with a valid hand-off, a 90% report, an
    /// idle pane and an empty box. The limit is 80% and the setting is on.
    async fn armed() -> (App, Panes, Bus, Instant) {
        let (mut app, panes, bus, now) = staffed().await;
        enable_transitions(&mut app);
        let recorded = record_handoff(
            &mut app,
            &panes,
            "cedar",
            ManagedRole::SessionOverlord,
            vec!["none".into()],
        );
        assert_eq!(recorded["result"]["valid"], true, "{recorded}");
        assert!(app.report_context_usage(&panes["cedar"].1, 90, "test adapter", now));
        (app, panes, bus, now)
    }

    #[tokio::test]
    async fn context_transition_runs_the_native_reset_once_when_every_guard_passes() {
        let (mut app, panes, mut bus, now) = armed().await;
        let cedar = panes["cedar"].1.clone();
        // Recording the hand-off and the report are model-reachable; neither types anything.
        append_memory(
            &mut app,
            &panes,
            "cedar",
            ManagedRole::SessionOverlord,
            "note",
        );
        assert_eq!(drain(bus.get_mut("cedar").unwrap()), "");

        app.advance_context_transitions(now + Duration::from_secs(10));
        let sent = submitted(bus.get_mut("cedar").unwrap()).await;
        assert_eq!(sent, "/new\r");
        assert!(app.overseer_service.transitions[&cedar]
            .status
            .starts_with("sent /new"));

        // The figure belonged to the conversation just reset: nothing more is sent.
        app.advance_context_transitions(now + Duration::from_secs(20));
        assert_eq!(drain(bus.get_mut("cedar").unwrap()), "");
        assert!(app
            .context_card(&cedar, now + Duration::from_secs(20))
            .contains("context: unknown"));
        for other in ["alder", "birch", "dune", "ember"] {
            assert_eq!(drain(bus.get_mut(other).unwrap()), "", "{other}");
        }
    }

    #[tokio::test]
    async fn context_transition_waits_for_an_idle_pane() {
        let (mut app, panes, mut bus, now) = armed().await;
        let cedar = panes["cedar"].1.clone();
        app.state.terminals.get_mut(&cedar).unwrap().state = AgentState::Working;
        app.advance_context_transitions(now + Duration::from_secs(10));
        assert_eq!(drain(bus.get_mut("cedar").unwrap()), "");
        assert_eq!(
            app.overseer_service.transitions[&cedar].status,
            "blocked: pane busy"
        );
        app.state.terminals.get_mut(&cedar).unwrap().state = AgentState::Idle;
        app.advance_context_transitions(now + Duration::from_secs(11));
        assert_eq!(submitted(bus.get_mut("cedar").unwrap()).await, "/new\r");
    }

    #[tokio::test]
    async fn context_transition_never_types_over_a_draft() {
        let (mut app, panes, mut bus, now) = armed().await;
        let cedar = panes["cedar"].1.clone();
        app.terminal_runtimes
            .get(&cedar)
            .unwrap()
            .test_process_pty_bytes(b"\x1b[2J\x1b[Hdone\r\n\xe2\x80\xba half a thought\r\n");
        app.advance_context_transitions(now + Duration::from_secs(10));
        assert_eq!(drain(bus.get_mut("cedar").unwrap()), "");
        assert_eq!(
            app.overseer_service.transitions[&cedar].status,
            "blocked: input draft present"
        );
        clear(&app, &cedar);
        app.advance_context_transitions(now + Duration::from_secs(11));
        assert_eq!(submitted(bus.get_mut("cedar").unwrap()).await, "/new\r");
    }

    #[tokio::test]
    async fn context_transition_asks_for_a_handoff_and_resets_only_on_a_valid_one() {
        let (mut app, panes, mut bus, now) = armed().await;
        let cedar = panes["cedar"].1.clone();
        // A newer, invalid hand-off (no findings entry) replaces the good one.
        let invalid = record_handoff(
            &mut app,
            &panes,
            "cedar",
            ManagedRole::SessionOverlord,
            Vec::new(),
        );
        assert_eq!(invalid["result"]["valid"], false);
        app.advance_context_transitions(now + Duration::from_secs(10));
        let asked = drain(bus.get_mut("cedar").unwrap());
        assert!(asked.contains("handoff record"), "{asked}");
        assert!(asked.contains("do not clear yourself"), "{asked}");
        assert!(!asked.contains("/new"), "{asked}");
        // One request per interval.
        app.advance_context_transitions(now + Duration::from_secs(20));
        assert_eq!(drain(bus.get_mut("cedar").unwrap()), "");
        assert_eq!(
            app.overseer_service.transitions[&cedar].status,
            "hand-off requested, waiting for a valid one"
        );
        // A valid one for the live conversation lets the reset run.
        let good = record_handoff(
            &mut app,
            &panes,
            "cedar",
            ManagedRole::SessionOverlord,
            vec!["none".into()],
        );
        assert_eq!(good["result"]["valid"], true);
        app.advance_context_transitions(now + Duration::from_secs(30));
        assert_eq!(submitted(bus.get_mut("cedar").unwrap()).await, "/new\r");
    }

    #[tokio::test]
    async fn context_transition_treats_a_corrupt_memory_line_as_no_valid_handoff() {
        let (mut app, panes, mut bus, now) = armed().await;
        let (_, path) = app
            .role_memory_scope_path(&panes["cedar"].1, ManagedRole::SessionOverlord)
            .unwrap();
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(b"{not json\n")
            .unwrap();
        app.advance_context_transitions(now + Duration::from_secs(10));
        let sent = drain(bus.get_mut("cedar").unwrap());
        assert!(!sent.contains("/new"), "{sent}");
        assert!(sent.contains("handoff record"), "{sent}");
    }

    #[tokio::test]
    async fn context_transition_stays_idle_without_a_report_a_fresh_one_or_the_setting() {
        // No report: unknown, nothing acts.
        let (mut app, panes, mut bus, now) = staffed().await;
        enable_transitions(&mut app);
        let cedar = panes["cedar"].1.clone();
        record_handoff(
            &mut app,
            &panes,
            "cedar",
            ManagedRole::SessionOverlord,
            vec!["none".into()],
        );
        app.advance_context_transitions(now + Duration::from_secs(10));
        assert_eq!(drain(bus.get_mut("cedar").unwrap()), "");
        assert_eq!(
            app.overseer_service.transitions[&cedar].status,
            "waiting: context use unknown"
        );
        // A report that is too old is unknown again.
        assert!(app.report_context_usage(&cedar, 95, "test adapter", now));
        app.advance_context_transitions(now + Duration::from_secs(10 * 60));
        assert_eq!(drain(bus.get_mut("cedar").unwrap()), "");
        // A report from another conversation is unknown too.
        assert!(app.report_context_usage(
            &cedar,
            95,
            "test adapter",
            now + Duration::from_secs(600)
        ));
        app.state
            .terminals
            .get_mut(&cedar)
            .unwrap()
            .set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
                source: "test".into(),
                agent: "codex".into(),
                session_ref: crate::agent_resume::AgentSessionRef::id("another-conversation")
                    .unwrap(),
            });
        app.advance_context_transitions(now + Duration::from_secs(610));
        assert_eq!(drain(bus.get_mut("cedar").unwrap()), "");

        // Setting off (the default): every guard satisfied, nothing acts.
        let (mut app, panes, mut bus, now) = armed().await;
        app.overseer_service.config.context_transition_enabled = false;
        app.advance_context_transitions(now + Duration::from_secs(10));
        assert_eq!(drain(bus.get_mut("cedar").unwrap()), "");
        assert!(app
            .context_card(&panes["cedar"].1, now)
            .contains("native transition=off by default"));
    }

    #[tokio::test]
    async fn context_transition_sends_nothing_to_a_harness_without_accepted_lifecycle() {
        for agent in [Agent::Pi, Agent::Cursor] {
            let (mut app, panes, mut bus, now) = armed().await;
            let cedar = panes["cedar"].1.clone();
            app.state.terminals.get_mut(&cedar).unwrap().detected_agent = Some(agent);
            app.advance_context_transitions(now + Duration::from_secs(10));
            assert_eq!(drain(bus.get_mut("cedar").unwrap()), "", "{agent:?}");
            assert!(
                app.overseer_service.transitions[&cedar]
                    .status
                    .starts_with("unsupported:"),
                "{agent:?}"
            );
        }
    }

    #[tokio::test]
    async fn context_transition_is_driven_by_the_overseer_tick() {
        let (mut app, _panes, mut bus, now) = armed().await;
        app.advance_overseer(now + Duration::from_secs(10));
        assert!(
            drain(bus.get_mut("cedar").unwrap()).contains("/new"),
            "the tick must reach the caller"
        );
    }

    #[test]
    fn effort_changes_never_ask_for_a_clear() {
        for (old, new) in [("low", "high"), ("high", "low"), ("medium", "medium")] {
            assert!(!crate::role_continuity::effort_change_requires_clear(
                old, new
            ));
        }
    }

    #[tokio::test]
    async fn role_card_shows_latest_entries_with_counts_and_names_every_path() {
        let (mut app, panes, _bus, _now) = staffed().await;
        let alder = panes["alder"].1.clone();
        assert!(app.state.set_session_overlord(&alder, true));
        let long = "x".repeat(600);
        for index in 0..5 {
            for role in [ManagedRole::SpaceOverseer, ManagedRole::SessionOverlord] {
                append_memory(&mut app, &panes, "alder", role, &format!("{index} {long}"));
            }
        }
        let card = app.role_continuity_card(&alder);
        assert_eq!(
            card.matches("latest 3 of 5 memory entries").count(),
            2,
            "{card}"
        );
        for role in [ManagedRole::SpaceOverseer, ManagedRole::SessionOverlord] {
            let (_, path) = app.role_memory_scope_path(&alder, role).unwrap();
            assert!(card.contains(&path.display().to_string()), "{card}");
        }
        assert_eq!(card.matches("full continuity at").count(), 2, "{card}");
        assert!(card.contains("\"4 x"), "newest entry is shown");
        assert!(!card.contains("\"0 x"), "oldest entry is not shown");
    }

    #[tokio::test]
    async fn role_card_reports_an_unreadable_memory_line_and_keeps_the_rest() {
        let (mut app, panes, _bus, _now) = staffed().await;
        let alder = panes["alder"].1.clone();
        append_memory(
            &mut app,
            &panes,
            "alder",
            ManagedRole::SpaceOverseer,
            "before damage",
        );
        let (_, path) = app
            .role_memory_scope_path(&alder, ManagedRole::SpaceOverseer)
            .unwrap();
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{damaged\n")
            .unwrap();
        append_memory(
            &mut app,
            &panes,
            "alder",
            ManagedRole::SpaceOverseer,
            "after damage",
        );
        let card = app.role_continuity_card(&alder);
        assert!(card.contains("memory unreadable: line 2"), "{card}");
        assert!(card.contains("before damage"), "{card}");
        assert!(card.contains("after damage"), "{card}");
        assert!(card.contains("latest 2 of 2"), "{card}");

        // A file that cannot be read at all says so instead of looking empty.
        std::fs::rename(&path, path.with_extension("damaged")).unwrap();
        std::fs::create_dir(&path).unwrap();
        let card = app.role_continuity_card(&alder);
        assert!(card.contains("memory unreadable: cannot read"), "{card}");
        assert!(!card.contains("latest memory=none recorded"), "{card}");
    }

    #[tokio::test]
    async fn role_card_declares_the_adapter_lifecycle_per_harness() {
        let (mut app, panes, _bus, _now) = staffed().await;
        let alder = panes["alder"].1.clone();
        let metadata = app.role_metadata(&alder);
        assert!(
            metadata.contains("adapter lifecycle: supported (SessionStart"),
            "{metadata}"
        );
        assert!(
            metadata.contains("native transition=off by default"),
            "{metadata}"
        );
        assert!(metadata.contains("context: unknown"), "{metadata}");
        for (agent, reason) in [
            (Agent::Pi, "no orientation-context response path"),
            (Agent::Cursor, "no orientation or context response"),
        ] {
            app.state.terminals.get_mut(&alder).unwrap().detected_agent = Some(agent);
            let metadata = app.role_metadata(&alder);
            assert!(
                metadata.contains("adapter lifecycle: unsupported (") && metadata.contains(reason),
                "{metadata}"
            );
        }
    }

    // ---- context meter from the Claude status line ----

    fn statusline_call(
        panes: &Panes,
        name: &str,
        percent: Option<u8>,
        session: Option<&str>,
    ) -> crate::api::schema::Request {
        crate::api::schema::Request {
            id: "statusline".into(),
            method: crate::api::schema::Method::PaneReportMetadata(
                crate::api::schema::PaneReportMetadataParams {
                    pane_id: format!("p_{}", panes[name].0.raw()),
                    source: "herdr:claude-statusline".into(),
                    agent: None,
                    applies_to_source: None,
                    title: None,
                    display_agent: None,
                    custom_status: None,
                    state_labels: HashMap::new(),
                    clear_title: true,
                    clear_display_agent: false,
                    clear_custom_status: false,
                    clear_state_labels: false,
                    context_used_percent: percent,
                    session_id: session.map(str::to_string),
                    context_unknown: false,
                    seq: None,
                    ttl_ms: None,
                },
            ),
        }
    }

    #[tokio::test]
    async fn status_line_context_reaches_the_meter_for_its_own_conversation_only() {
        let (mut app, panes, _bus, _now) = staffed().await;
        let alder = panes["alder"].1.clone();
        let cedar = panes["cedar"].1.clone();
        let reply = app.handle_api_request(statusline_call(
            &panes,
            "alder",
            Some(42),
            Some("alder-session"),
        ));
        assert!(!reply.contains("error"), "{reply}");
        let later = Instant::now();
        let report = app
            .context_report(&alder, later)
            .expect("reading for alder");
        assert_eq!(report.percent, 42);
        assert_eq!(report.source, "Claude status line");
        assert!(app
            .context_card(&alder, later)
            .starts_with("context: 42% (Claude status line, age "));
        assert!(app.context_report(&cedar, later).is_none());
        assert!(app
            .context_card(&cedar, later)
            .starts_with("context: unknown"));
    }

    #[tokio::test]
    async fn status_line_without_context_keeps_the_meter_unknown() {
        let (mut app, panes, _bus, _now) = staffed().await;
        let alder = panes["alder"].1.clone();
        let reply = app.handle_api_request(statusline_call(
            &panes,
            "alder",
            None,
            Some("alder-session"),
        ));
        assert!(!reply.contains("error"), "{reply}");
        assert!(app.context_report(&alder, Instant::now()).is_none());
        assert!(app
            .context_card(&alder, Instant::now())
            .starts_with("context: unknown"));
    }

    #[tokio::test]
    async fn status_line_context_for_another_or_missing_conversation_is_ignored() {
        let (mut app, panes, _bus, _now) = staffed().await;
        let alder = panes["alder"].1.clone();
        for (percent, session) in [
            (90, Some("someone-elses-session")),
            (90, Some("cedar-session")),
            (90, None),
            (90, Some("  ")),
            (101, Some("alder-session")),
        ] {
            app.handle_api_request(statusline_call(&panes, "alder", Some(percent), session));
            assert!(
                app.context_report(&alder, Instant::now()).is_none(),
                "{percent} {session:?}"
            );
        }
        // A reading kept for the old conversation stops counting once the conversation changes.
        app.handle_api_request(statusline_call(
            &panes,
            "alder",
            Some(55),
            Some("alder-session"),
        ));
        assert!(app.context_report(&alder, Instant::now()).is_some());
        app.state
            .terminals
            .get_mut(&alder)
            .unwrap()
            .set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
                source: "test".into(),
                agent: "codex".into(),
                session_ref: crate::agent_resume::AgentSessionRef::id("alder-next-session")
                    .unwrap(),
            });
        assert!(app.context_report(&alder, Instant::now()).is_none());
    }

    #[tokio::test]
    async fn status_line_context_reading_expires() {
        let (mut app, panes, _bus, _now) = staffed().await;
        let alder = panes["alder"].1.clone();
        app.handle_api_request(statusline_call(
            &panes,
            "alder",
            Some(61),
            Some("alder-session"),
        ));
        let fresh = Instant::now();
        assert!(app
            .context_report(&alder, fresh + Duration::from_secs(60))
            .is_some());
        let stale = fresh + Duration::from_secs(6 * 60);
        assert!(app.context_report(&alder, stale).is_none());
        assert!(app
            .context_card(&alder, stale)
            .starts_with("context: unknown"));
    }

    #[tokio::test]
    async fn status_line_context_feeds_the_transition_for_a_valid_handoff() {
        let (mut app, panes, mut bus, _now) = armed().await;
        // Replace the test report with one that arrives the way the hook sends it.
        app.overseer_service.context_reports.clear();
        app.handle_api_request(statusline_call(
            &panes,
            "cedar",
            Some(91),
            Some("cedar-session"),
        ));
        let cedar = panes["cedar"].1.clone();
        // The metadata event re-reads the fixture screen, which is not a real agent
        // screen, so the pane state is set back to idle as the harness would report it.
        app.state.terminals.get_mut(&cedar).unwrap().state = AgentState::Idle;
        let tick = Instant::now() + Duration::from_secs(10);
        assert!(app.context_report(&cedar, tick).is_some(), "reading stored");
        app.advance_context_transitions(tick);
        let status = app.overseer_service.transitions[&cedar].status.clone();
        assert_eq!(drain(bus.get_mut("cedar").unwrap()), "/new\r", "{status}");
    }

    fn unknown_call(
        panes: &Panes,
        name: &str,
        session: Option<&str>,
    ) -> crate::api::schema::Request {
        let mut request = statusline_call(panes, name, None, session);
        if let crate::api::schema::Method::PaneReportMetadata(params) = &mut request.method {
            params.context_unknown = true;
        }
        request
    }

    fn metadata_patch(panes: &Panes, name: &str, title: &str) -> crate::api::schema::Request {
        crate::api::schema::Request {
            id: "patch".into(),
            method: crate::api::schema::Method::PaneReportMetadata(
                crate::api::schema::PaneReportMetadataParams {
                    pane_id: format!("p_{}", panes[name].0.raw()),
                    source: "user:title".into(),
                    agent: None,
                    applies_to_source: None,
                    title: Some(title.into()),
                    display_agent: None,
                    custom_status: None,
                    state_labels: HashMap::new(),
                    clear_title: false,
                    clear_display_agent: false,
                    clear_custom_status: false,
                    clear_state_labels: false,
                    context_used_percent: None,
                    session_id: None,
                    context_unknown: false,
                    seq: None,
                    ttl_ms: None,
                },
            ),
        }
    }

    #[tokio::test]
    async fn known_reading_then_unknown_clears_it_and_blocks_the_reset() {
        let (mut app, panes, mut bus, now) = staffed().await;
        enable_transitions(&mut app);
        let cedar = panes["cedar"].1.clone();
        let recorded = record_handoff(
            &mut app,
            &panes,
            "cedar",
            ManagedRole::SessionOverlord,
            vec!["none".into()],
        );
        assert_eq!(recorded["result"]["valid"], true, "{recorded}");
        // A null or a missing reading reaches the server the same way: session-bound unknown.
        for label in ["null", "missing"] {
            app.handle_api_request(statusline_call(
                &panes,
                "cedar",
                Some(92),
                Some("cedar-session"),
            ));
            assert!(
                app.context_report(&cedar, now).is_some(),
                "{label}: 92 stored"
            );
            let reply =
                app.handle_api_request(unknown_call(&panes, "cedar", Some("cedar-session")));
            assert!(!reply.contains("error"), "{label}: {reply}");
            assert!(
                app.context_report(&cedar, Instant::now()).is_none(),
                "{label}: 92 cleared"
            );
            assert!(
                app.context_card(&cedar, Instant::now())
                    .starts_with("context: unknown"),
                "{label}"
            );
            app.state.terminals.get_mut(&cedar).unwrap().state = AgentState::Idle;
            app.advance_context_transitions(now + Duration::from_secs(10));
            assert_eq!(
                drain(bus.get_mut("cedar").unwrap()),
                "",
                "{label}: no reset"
            );
            assert_eq!(
                app.overseer_service.transitions[&cedar].status, "waiting: context use unknown",
                "{label}"
            );
        }
    }

    #[tokio::test]
    async fn another_conversations_unknown_and_unrelated_patches_leave_the_reading() {
        let (mut app, panes, _bus, now) = staffed().await;
        let cedar = panes["cedar"].1.clone();
        app.handle_api_request(statusline_call(
            &panes,
            "cedar",
            Some(92),
            Some("cedar-session"),
        ));
        let reading = |app: &App| {
            app.context_report(&cedar, Instant::now())
                .map(|r| r.percent)
        };

        app.handle_api_request(unknown_call(&panes, "cedar", Some("someone-elses-session")));
        assert_eq!(reading(&app), Some(92), "another conversation's unknown");
        app.handle_api_request(unknown_call(&panes, "cedar", None));
        assert_eq!(reading(&app), Some(92), "unknown without a session");
        app.handle_api_request(unknown_call(&panes, "alder", Some("cedar-session")));
        assert_eq!(
            reading(&app),
            Some(92),
            "same conversation id on another pane"
        );
        let patch = app.handle_api_request(metadata_patch(&panes, "cedar", "Renamed"));
        assert!(!patch.contains("error"), "{patch}");
        assert_eq!(reading(&app), Some(92), "unrelated metadata patch");
        // Valid zero is a reading, not unknown.
        app.handle_api_request(statusline_call(
            &panes,
            "cedar",
            Some(0),
            Some("cedar-session"),
        ));
        assert_eq!(reading(&app), Some(0));
        assert!(app.context_report(&cedar, now).is_some());
    }
}

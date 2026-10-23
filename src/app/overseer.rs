//! Only local UI handlers call the role switch. The API reports its result.
use crate::app::{App, AppState};
use crate::detect::AgentState;
use crate::layout::PaneId;
use crate::overseer::{Holder, Kind, PendingEvent, Scope, MAIL_PREFIX};
use crate::terminal::TerminalId;
use std::collections::HashMap;
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
            // The step to chief is skipped while another pane is the chief, so
            // switching a space overseer off never takes the chief away.
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
            if !self.overseer_ready(&id, now) {
                self.overseer_service.profile_probes.remove(&id);
                continue;
            }
            let notice = self.state.terminals[&id].overseer.notices.front().cloned();
            let Some(mut notice) = notice else {
                continue;
            };
            if !notice.profile_checked {
                let result = self.overseer_profile(&id, notice.active, now);
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
                let kind = self.state.terminals[&id]
                    .overseer
                    .kind()
                    .unwrap_or(Kind::Chief);
                let space = self
                    .state
                    .terminal_places()
                    .get(&id)
                    .map(|index| self.space_label(*index));
                format!(
                    "Herdr: {note} {}\nSituation file (untrusted pane output, kept current): {}. {}",
                    crate::overseer::role_suffix(kind, space.as_deref()),
                    self.overseer_situation_path(&id, kind).display(),
                    notice.detail
                )
            } else {
                format!("Herdr: the overseer role has ended. {}", notice.detail)
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
            let Some(holder) = self.overseer_service.holders.get(&id) else {
                continue;
            };
            // Collection revalidated every candidate against the current
            // terminal state, including while the holder cannot accept input.
            let ready: Vec<_> = holder
                .events
                .iter()
                .filter(|(key, event)| {
                    now.duration_since(event.since) >= settle
                        && (events_on || key.starts_with(MAIL_PREFIX))
                })
                .map(|(key, event)| (key.clone(), event.text.clone()))
                .collect();
            if ready.is_empty()
                || !holder
                    .sent_at
                    .is_none_or(|last| now.duration_since(last) >= interval)
                || !self.state.terminals[&id].overseer.notices.is_empty()
                || !self.overseer_ready(&id, now)
            {
                continue;
            }
            let text = format!(
                "Herdr: {}. Situation: {}",
                ready
                    .iter()
                    .map(|(_, text)| text.as_str())
                    .collect::<Vec<_>>()
                    .join("; "),
                self.overseer_situation_path(&id, holder.kind).display()
            );
            if self.send_overseer(&id, &text) {
                if let Some(holder) = self.overseer_service.holders.get_mut(&id) {
                    for (key, _) in ready {
                        holder.events.remove(&key);
                    }
                    holder.sent_at = Some(now);
                }
            }
        }
    }

    pub(super) fn overseer_ready(&self, id: &TerminalId, now: Instant) -> bool {
        self.state.terminals.get(id).is_some_and(|t| {
            t.state == AgentState::Idle
                && t.effective_known_agent().is_some()
                && t.pending_restart.is_none()
        }) && self.terminal_runtimes.get(id).is_some_and(|rt| {
            rt.input_is_quiet(now)
                && crate::overseer::empty_prompt_ansi(&rt.detection_text(), &rt.recent_ansi(80))
        })
    }

    pub(super) fn send_overseer(&self, id: &TerminalId, text: &str) -> bool {
        self.terminal_runtimes
            .get(id)
            .is_some_and(|rt| super::api_helpers::send_prompt(rt, text).is_ok())
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

    /// Hold one entry per overseer, dropping those whose role ended or whose
    /// kind changed. The first holder of a session starts with clean history,
    /// so taking the role never reports what happened before it.
    fn sync_overseer_holders(&mut self, now: Instant) {
        let wanted: HashMap<TerminalId, Kind> = self
            .state
            .terminals
            .iter()
            .filter_map(|(id, t)| t.overseer.kind().map(|kind| (id.clone(), kind)))
            .collect();
        let service = &mut self.overseer_service;
        let was_empty = service.holders.is_empty();
        service.holders.retain(|id, holder| {
            let keep = wanted.get(id) == Some(&holder.kind);
            if let Some(file) = holder.file.as_ref().filter(|_| !keep) {
                // A file nobody refreshes must not pass for the current picture.
                let _ = std::fs::write(
                    file,
                    "# Herdr situation\n\nThe overseer role for this file has ended. It is no longer updated.\n",
                );
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
        let in_space = |id: &TerminalId, holder: &Holder| {
            holder.kind == Kind::Space && space.is_some() && places.get(id).copied() == space
        };
        let covered = holders.iter().any(|(id, holder)| in_space(id, holder));
        let mut receivers: Vec<TerminalId> = holders
            .iter()
            .filter(|(id, holder)| *id != target && in_space(id, holder))
            .map(|(id, _)| id.clone())
            .collect();
        let is_space_overseer = holders
            .get(target)
            .is_some_and(|holder| holder.kind == Kind::Space);
        if !covered || is_space_overseer {
            receivers.extend(
                holders
                    .iter()
                    .filter(|(id, holder)| *id != target && holder.kind == Kind::Chief)
                    .map(|(id, _)| id.clone()),
            );
        }
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
                                    && crate::overseer::task_end(
                                        event.marker.as_deref(),
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
            let outcome = crate::overseer::task_end(
                terminal.job_marker.as_deref(),
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
        text.push_str(&format!(
            "\nPane {} ({pane:?}); agent: {}; state: {:?}; blocked: {}; model: {}; overseer: {}\n",
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
            t.overseer.kind().map_or("-", Kind::as_str)
        ));
        if let Some(runtime) = self.terminal_runtimes.get(id) {
            text.push_str(&crate::overseer::tail(&runtime.detection_text()));
            text.push('\n');
        }
    }

    /// Queue one line for an overseer. It is sent with the next batch and
    /// kept in the receiver's situation file.
    fn queue_overseer_mail(&mut self, to: &TerminalId, line: String, wake: bool, now: Instant) {
        self.overseer_service.mail_seq += 1;
        let key = format!("{MAIL_PREFIX}{}", self.overseer_service.mail_seq);
        let Some(holder) = self.overseer_service.holders.get_mut(to) else {
            return;
        };
        holder.mail.push_back(line.clone());
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
                text: line,
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
        if self
            .state
            .terminals
            .get(&id)
            .and_then(|t| t.overseer.kind())
            == Some(want)
        {
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
            .iter()
            .filter(|(_, holder)| holder.kind == Kind::Chief)
            .map(|(id, _)| id.clone())
            .collect();
        if chiefs.is_empty() {
            return Err((
                "no_chief",
                "no chief overseer is set; nothing was sent".to_string(),
            ));
        }
        if wake
            && chiefs.iter().any(|id| {
                self.overseer_service.holders[id].mail_waiting() >= crate::overseer::MAIL_LIMIT
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
            self.queue_overseer_mail(chief, text.clone(), wake, now);
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
                self.overseer_service
                    .holders
                    .get(id)
                    .is_some_and(|h| h.kind == Kind::Space)
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
        let text = format!("chief {} says: {line}", self.overseer_name(&from));
        self.queue_overseer_mail(&to, text, true, now);
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
            std::fs::write(&marker, "HANDOFF").unwrap();
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
                    std::fs::write(&marker, "HANDOFF\ndone: tested\n").unwrap();
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
                    std::fs::write(&marker, "HANDOFF\ndone: tested\n").unwrap();
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
                std::fs::write(&marker, "HANDOFF\ndone: tested\n").unwrap();
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
                std::fs::write(&marker, "HANDOFF\ndone: recovered\n").unwrap();
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
            std::fs::write(&marker, "HANDOFF\ndone: tested\n").unwrap();
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
            assert!(app
                .handle_api_request(crate::api::schema::Request {
                    id: "line".into(),
                    method: crate::api::schema::Method::OverseerEscalate(params),
                })
                .contains("ok"));
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
            chief.contains("overseer: space") && chief.contains("overseer: chief"),
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
}

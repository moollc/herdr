//! Lines between overseers. Only the overseers named by the caller's role can
//! send them, and a space overseer can only reach the chief.
use super::responses::{encode_error, encode_success};
use crate::api::schema::{
    ManagedRole, OverseerEscalateParams, OverseerMessageParams, ResponseResult,
    RoleHandoffRecordParams, RoleHolderInfo, RoleMemoryAppendParams, RoleMemoryPathInfo,
    RoleSetParams, RoleStatusInfo, RoleStatusParams, SpaceRoleInfo,
};
use crate::app::App;
use crate::overseer::Kind;
use sha2::{Digest, Sha256};

const DELIVERY_KEY_MAX_CHARS: usize = 128;

impl App {
    pub(super) fn handle_role_handoff_record(
        &mut self,
        id: String,
        params: RoleHandoffRecordParams,
    ) -> String {
        let Some(caller) = self.parse_reporting_pane_id(&params.caller_pane_id) else {
            return encode_error(id, "pane_not_found", "calling pane not found");
        };
        let Some(terminal_id) = self.state.terminal_id_for_any_pane(caller) else {
            return encode_error(id, "pane_not_found", "calling terminal not found");
        };
        let Some(terminal) = self.state.terminals.get(&terminal_id) else {
            return encode_error(id, "pane_not_found", "calling terminal not found");
        };
        let holds = match params.role {
            ManagedRole::SpaceOverseer => terminal.overseer.is_space_overseer(),
            ManagedRole::SessionOverlord => terminal.overseer.is_session_overlord(),
        };
        if !holds {
            return encode_error(id, "role_required", "caller does not hold this role");
        }
        let Some(native_session) = terminal.current_session_identity_for_persistence() else {
            return encode_error(
                id,
                "native_session_required",
                "a native conversation is required",
            );
        };
        let Some((_, path)) = self.role_memory_scope_path(&terminal_id, params.role) else {
            return encode_error(id, "role_scope_missing", "role scope is not available");
        };
        let recorded_at_unix = crate::role_continuity::unix_now();
        let packet = crate::role_continuity::HandoffPacket {
            task_id: params.task_id,
            attempt: params.attempt,
            candidate: params.candidate,
            tests: params.tests,
            output_paths: params.output_paths,
            open_findings: params.open_findings,
            cost: params.cost,
            native_session: native_session.clone(),
            next_owner: params.next_owner,
            recorded_at_unix,
        };
        let checked =
            crate::role_continuity::check_handoff(&packet, recorded_at_unix, Some(&native_session));
        let record = crate::role_continuity::ContinuityRecord::Handoff { packet };
        if let Err(error) = crate::role_continuity::append_record(&path, &record) {
            return encode_error(
                id,
                "working_memory_write_failed",
                format!("could not append hand-off: {error}"),
            );
        }
        encode_success(
            id,
            ResponseResult::RoleHandoffRecorded {
                valid: checked.valid,
                reason: checked.reason,
                recorded_at_unix,
            },
        )
    }
    pub(in crate::app) fn role_memory_scope_path(
        &self,
        terminal_id: &crate::terminal::TerminalId,
        role: ManagedRole,
    ) -> Option<(String, std::path::PathBuf)> {
        let scope_id = match role {
            ManagedRole::SessionOverlord => "session".to_string(),
            ManagedRole::SpaceOverseer => {
                let index = self.state.terminal_places().get(terminal_id).copied()?;
                self.state.workspaces.get(index)?.id.clone()
            }
        };
        let scope_key = match role {
            ManagedRole::SessionOverlord => "session:overlord".to_string(),
            ManagedRole::SpaceOverseer => format!("space:{scope_id}"),
        };
        let root = self
            .overseer_service
            .situation_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("role-memory");
        Some((
            scope_id,
            crate::role_continuity::memory_path(&root, &scope_key),
        ))
    }

    fn role_memory_paths(
        &self,
        terminal_id: &crate::terminal::TerminalId,
    ) -> Vec<RoleMemoryPathInfo> {
        let Some(terminal) = self.state.terminals.get(terminal_id) else {
            return Vec::new();
        };
        [
            (
                ManagedRole::SpaceOverseer,
                terminal.overseer.is_space_overseer(),
            ),
            (
                ManagedRole::SessionOverlord,
                terminal.overseer.is_session_overlord(),
            ),
        ]
        .into_iter()
        .filter(|(_, enabled)| *enabled)
        .filter_map(|(role, _)| {
            self.role_memory_scope_path(terminal_id, role)
                .map(|(scope_id, path)| RoleMemoryPathInfo {
                    role,
                    scope_id,
                    path: path.display().to_string(),
                })
        })
        .collect()
    }

    pub(super) fn handle_role_memory_append(
        &mut self,
        id: String,
        params: RoleMemoryAppendParams,
    ) -> String {
        let Some(caller) = self.parse_reporting_pane_id(&params.caller_pane_id) else {
            return encode_error(id, "pane_not_found", "calling pane not found");
        };
        let Some(terminal_id) = self.state.terminal_id_for_any_pane(caller) else {
            return encode_error(id, "pane_not_found", "calling terminal not found");
        };
        let Some(terminal) = self.state.terminals.get(&terminal_id) else {
            return encode_error(id, "pane_not_found", "calling terminal not found");
        };
        let holds_role = match params.role {
            ManagedRole::SpaceOverseer => terminal.overseer.is_space_overseer(),
            ManagedRole::SessionOverlord => terminal.overseer.is_session_overlord(),
        };
        if !holds_role {
            return encode_error(id, "role_required", "caller does not hold this role");
        }
        if params.text.trim().is_empty()
            || params.text.chars().count() > crate::role_continuity::MEMORY_ENTRY_MAX_CHARS
        {
            return encode_error(
                id,
                "invalid_memory_entry",
                "working-memory entries must contain 1 to 2048 characters",
            );
        }
        let Some((scope_id, path)) = self.role_memory_scope_path(&terminal_id, params.role) else {
            return encode_error(id, "role_scope_missing", "role scope is not available");
        };
        let native_session = terminal.current_session_identity_for_persistence();
        let recorded_at_unix = crate::role_continuity::unix_now();
        let record = crate::role_continuity::ContinuityRecord::Memory {
            recorded_at_unix,
            role: format!("{:?}", params.role).to_lowercase(),
            scope_id: scope_id.clone(),
            terminal_id: terminal_id.to_string(),
            native_session,
            text: params.text,
        };
        if let Err(error) = crate::role_continuity::append_record(&path, &record) {
            return encode_error(
                id,
                "working_memory_write_failed",
                format!("could not append role memory: {error}"),
            );
        }
        encode_success(
            id,
            ResponseResult::RoleMemoryAppended {
                role: params.role,
                scope_id,
                path: path.display().to_string(),
                recorded_at_unix,
            },
        )
    }

    fn delivery_binding_sha256(
        &self,
        sender: &crate::terminal::TerminalId,
        sender_role: Kind,
        receivers: &[crate::terminal::TerminalId],
        receiver_role: Kind,
        direction: &str,
        text: &str,
        wake: bool,
    ) -> Option<String> {
        let authority = |terminal_id: &crate::terminal::TerminalId, role: Kind| {
            let terminal = self.state.terminals.get(terminal_id)?;
            let holds = match role {
                Kind::Space => terminal.overseer.is_space_overseer(),
                Kind::Chief => terminal.overseer.is_session_overlord(),
            };
            holds.then(|| {
                let workspace = self
                    .state
                    .terminal_places()
                    .get(terminal_id)
                    .and_then(|index| self.state.workspaces.get(*index))
                    .map(|workspace| workspace.id.clone());
                serde_json::json!({
                    "terminal": terminal_id.to_string(),
                    "role": format!("{role:?}"),
                    "generation": terminal.overseer.generation(role),
                    "workspace": workspace,
                    "native_session": terminal.current_session_identity_for_persistence(),
                })
            })
        };
        let sender_binding = authority(sender, sender_role)?;
        let mut receiver_bindings = receivers
            .iter()
            .map(|terminal_id| authority(terminal_id, receiver_role))
            .collect::<Option<Vec<_>>>()?;
        receiver_bindings
            .sort_by_key(|binding| binding["terminal"].as_str().unwrap_or_default().to_string());
        let encoded = serde_json::to_vec(&serde_json::json!({
            "sender": sender_binding,
            "receivers": receiver_bindings,
            "direction": direction,
            "text": text,
            "wake": wake,
        }))
        .ok()?;
        Some(format!("{:x}", Sha256::digest(encoded)))
    }

    fn validate_delivery_key(key: &str) -> bool {
        !key.is_empty()
            && key.len() <= DELIVERY_KEY_MAX_CHARS
            && key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
    }

    fn remember_logical_delivery(
        &mut self,
        key: String,
        binding_sha256: String,
        message_ids: Vec<String>,
    ) {
        let service = &mut self.overseer_service;
        if service.logical_deliveries.contains_key(&key) {
            return;
        }
        service.logical_delivery_order.push_back(key.clone());
        service.logical_deliveries.insert(
            key,
            crate::overseer::LogicalDeliveryReceipt {
                binding_sha256,
                message_ids,
            },
        );
        while service.logical_delivery_order.len() > crate::overseer::LOGICAL_DELIVERY_HISTORY {
            if let Some(expired) = service.logical_delivery_order.pop_front() {
                service.logical_deliveries.remove(&expired);
            }
        }
    }

    fn delivery_receipt(&self, id: String, message_ids: Vec<String>) -> String {
        let mut target_terminal_ids = Vec::new();
        let mut state = None;
        for message_id in &message_ids {
            if let Some(delivery) = self.overseer_service.message_deliveries.get(message_id) {
                if !target_terminal_ids.contains(&delivery.target_terminal_id.to_string()) {
                    target_terminal_ids.push(delivery.target_terminal_id.to_string());
                }
                state.get_or_insert_with(|| delivery.state.clone());
            }
        }
        encode_success(
            id,
            ResponseResult::RoleMessageReceipt {
                message_ids,
                state: state.unwrap_or(crate::overseer::MessageDeliveryState::Queued),
                target_terminal_ids,
            },
        )
    }

    fn keyed_delivery_retry(
        &self,
        id: &str,
        key: Option<&str>,
        binding_sha256: Option<String>,
    ) -> Option<String> {
        let key = key?;
        let binding_sha256 = binding_sha256?;
        let prior = self.overseer_service.logical_deliveries.get(key)?;
        if prior.binding_sha256 != binding_sha256 {
            return Some(encode_error(
                id.to_string(),
                "delivery_key_reused",
                "delivery key is already bound to a different sender, receiver, authority, or payload",
            ));
        }
        Some(self.delivery_receipt(id.to_string(), prior.message_ids.clone()))
    }

    pub(super) fn handle_role_set(&mut self, id: String, params: RoleSetParams) -> String {
        let Some(_caller) = self.parse_reporting_pane_id(&params.caller_pane_id) else {
            return encode_error(id, "pane_not_found", "calling pane not found");
        };
        let target_id = params
            .target_pane_id
            .as_deref()
            .unwrap_or(&params.caller_pane_id);
        let Some(target) = self.parse_reporting_pane_id(target_id) else {
            return encode_error(id, "pane_not_found", "target pane not found");
        };
        let Some(target_terminal) = self.state.terminal_id_for_any_pane(target) else {
            return encode_error(id, "pane_not_found", "target terminal not found");
        };
        let changed = match params.role {
            ManagedRole::SpaceOverseer => self
                .state
                .set_space_overseer(&target_terminal, params.enabled),
            ManagedRole::SessionOverlord => self
                .state
                .set_session_overlord(&target_terminal, params.enabled),
        };
        if !changed {
            let same = self
                .state
                .terminals
                .get(&target_terminal)
                .is_some_and(|terminal| match params.role {
                    ManagedRole::SpaceOverseer => {
                        terminal.overseer.is_space_overseer() == params.enabled
                    }
                    ManagedRole::SessionOverlord => {
                        terminal.overseer.is_session_overlord() == params.enabled
                    }
                });
            if !same {
                return encode_error(
                    id,
                    "role_unavailable",
                    "space overseer requires a pane in a space",
                );
            }
        }
        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_role_status(&mut self, id: String, params: RoleStatusParams) -> String {
        let Some(caller) = self.parse_reporting_pane_id(&params.caller_pane_id) else {
            return encode_error(id, "pane_not_found", "calling pane not found");
        };
        let Some(caller_terminal) = self.state.terminal_id_for_any_pane(caller) else {
            return encode_error(id, "pane_not_found", "calling terminal not found");
        };
        let places = self.state.terminal_places();
        let caller_workspace_id = places
            .get(&caller_terminal)
            .map(|index| self.public_workspace_id(*index));
        let caller_public_pane_id = places
            .get(&caller_terminal)
            .and_then(|index| self.public_pane_id(*index, caller))
            .unwrap_or_else(|| params.caller_pane_id.clone());
        let caller_ui_focused = places.get(&caller_terminal).is_some_and(|index| {
            self.state.active == Some(*index)
                && self.state.workspaces[*index].focused_pane_id() == Some(caller)
        });
        let Some(terminal) = self.state.terminals.get(&caller_terminal) else {
            return encode_error(id, "pane_not_found", "calling terminal not found");
        };
        let caller_native_session = terminal.current_session_identity_for_persistence().map(
            |(source, agent, kind, value)| crate::api::schema::AgentSessionRef {
                source,
                agent,
                kind,
                value,
            },
        );
        let inspected_id = params
            .inspected_pane_id
            .as_deref()
            .unwrap_or(&params.caller_pane_id);
        let Some(inspected_pane) = self.parse_reporting_pane_id(inspected_id) else {
            return encode_error(id, "pane_not_found", "inspected pane not found");
        };
        let Some(inspected_terminal_id) = self.state.terminal_id_for_any_pane(inspected_pane)
        else {
            return encode_error(id, "pane_not_found", "inspected terminal not found");
        };
        let Some(inspected_terminal) = self.state.terminals.get(&inspected_terminal_id) else {
            return encode_error(id, "pane_not_found", "inspected terminal not found");
        };
        let inspected_workspace_id = places
            .get(&inspected_terminal_id)
            .map(|index| self.public_workspace_id(*index));
        let inspected_public_pane_id = places
            .get(&inspected_terminal_id)
            .and_then(|index| self.public_pane_id(*index, inspected_pane))
            .unwrap_or_else(|| inspected_id.to_string());
        let inspected_raw_pane_id = format!("p_{}", inspected_pane.raw());
        let observed_at_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let peers = self
            .state
            .terminals
            .iter()
            .map(|(terminal_id, peer)| {
                let location =
                    self.state
                        .workspaces
                        .iter()
                        .enumerate()
                        .find_map(|(ws_idx, workspace)| {
                            workspace
                                .tabs
                                .iter()
                                .enumerate()
                                .find_map(|(tab_idx, tab)| {
                                    tab.panes.iter().find_map(|(pane_id, pane)| {
                                        (&pane.attached_terminal_id == terminal_id).then(|| {
                                            (
                                                format!("p_{}", pane_id.raw()),
                                                self.public_pane_id(ws_idx, *pane_id),
                                                self.public_workspace_id(ws_idx),
                                                self.public_tab_id(ws_idx, tab_idx),
                                            )
                                        })
                                    })
                                })
                        });
                let native_session = peer.current_session_identity_for_persistence().map(
                    |(source, agent, kind, value)| crate::api::schema::AgentSessionRef {
                        source,
                        agent,
                        kind,
                        value,
                    },
                );
                let mut unknown = Vec::new();
                if peer.effective_agent_label().is_none() {
                    unknown.push("harness not detected".to_string());
                }
                if peer.model_info.is_none() {
                    unknown.push("observed model and effort unavailable".to_string());
                }
                if peer.job_marker.is_none() {
                    unknown.push("no task marker registered".to_string());
                }
                unknown.extend([
                    "task receipt validation is not exposed by the live role API".to_string(),
                    "latest candidate acceptance is not available from live state".to_string(),
                    "native acceptance is not available from live state".to_string(),
                    "missing batch members are not available from live state".to_string(),
                    "metadata age is not measured by this API".to_string(),
                ]);
                crate::api::schema::RolePeerStatusInfo {
                    terminal_id: terminal_id.to_string(),
                    raw_pane_id: location.as_ref().map(|value| value.0.clone()),
                    pane_id: location.as_ref().and_then(|value| value.1.clone()),
                    workspace_id: location.as_ref().map(|value| value.2.clone()),
                    tab_id: location.as_ref().and_then(|value| value.3.clone()),
                    is_caller: *terminal_id == caller_terminal,
                    space_overseer: peer.overseer.is_space_overseer(),
                    session_overlord: peer.overseer.is_session_overlord(),
                    role_revision: peer.overseer.revision,
                    harness: peer.effective_agent_label().map(str::to_string),
                    native_session,
                    lifecycle: format!("{:?}", peer.state).to_lowercase(),
                    observed_model: peer.model_info.as_ref().map(|info| info.model.clone()),
                    observed_effort: peer
                        .model_info
                        .as_ref()
                        .and_then(|info| info.effort.clone()),
                    configured_model_pin: peer.model_pin.as_ref().map(|pin| pin.model.clone()),
                    configured_effort_pin: peer
                        .model_pin
                        .as_ref()
                        .and_then(|pin| pin.effort.clone()),
                    task_marker: peer
                        .job_marker
                        .as_ref()
                        .map(|path| path.display().to_string()),
                    task_receipt: None,
                    latest_candidate: None,
                    native_acceptance: None,
                    missing_batch_members: None,
                    metadata_revision: peer.revision,
                    freshness: format!("observed at unix {observed_at_unix}; age unknown"),
                    unknown,
                }
            })
            .collect();
        let holders = |kind: ManagedRole| {
            self.state
                .terminals
                .iter()
                .filter(|(_, terminal)| match kind {
                    ManagedRole::SpaceOverseer => terminal.overseer.is_space_overseer(),
                    ManagedRole::SessionOverlord => terminal.overseer.is_session_overlord(),
                })
                .map(|(terminal_id, _)| {
                    let workspace_id = places
                        .get(terminal_id)
                        .map(|index| self.public_workspace_id(*index));
                    RoleHolderInfo {
                        terminal_id: terminal_id.to_string(),
                        workspace_id,
                    }
                })
                .collect::<Vec<_>>()
        };
        let overlords = holders(ManagedRole::SessionOverlord);
        let status = RoleStatusInfo {
            schema_version: 1,
            observed_at_unix,
            caller_terminal_id: caller_terminal.to_string(),
            caller_pane_id: params.caller_pane_id,
            caller_raw_pane_id: format!("p_{}", caller.raw()),
            caller_public_pane_id,
            caller_identity_source: "explicit pane identifier resolved to live terminal".into(),
            caller_workspace_id,
            caller_native_session,
            caller_ui_focused,
            inspected_terminal_id: inspected_terminal_id.to_string(),
            inspected_raw_pane_id,
            inspected_pane_id: inspected_public_pane_id,
            inspected_workspace_id,
            inspected_overseer: inspected_terminal.overseer.is_space_overseer(),
            inspected_overlord: inspected_terminal.overseer.is_session_overlord(),
            inspected_role_revision: inspected_terminal.overseer.revision,
            caller_overseer: terminal.overseer.is_space_overseer(),
            caller_overlord: terminal.overseer.is_session_overlord(),
            role_revision: terminal.overseer.revision,
            session_overlord: overlords.into_iter().next(),
            space_overseers: self
                .state
                .workspaces
                .iter()
                .enumerate()
                .map(|(index, _workspace)| {
                    let workspace_id = self.public_workspace_id(index);
                    let overseer = self
                        .state
                        .terminals
                        .iter()
                        .find(|(terminal_id, terminal)| {
                            places.get(*terminal_id) == Some(&index)
                                && terminal.overseer.is_space_overseer()
                        })
                        .map(|(terminal_id, _)| RoleHolderInfo {
                            terminal_id: terminal_id.to_string(),
                            workspace_id: Some(workspace_id.clone()),
                        });
                    SpaceRoleInfo {
                        workspace_id,
                        overseer,
                    }
                })
                .collect(),
            message_deliveries: self
                .overseer_service
                .message_deliveries
                .iter()
                .map(
                    |(message_id, delivery)| crate::api::schema::RoleMessageStatusInfo {
                        message_id: message_id.clone(),
                        target_terminal_id: delivery.target_terminal_id.to_string(),
                        state: delivery.state.clone(),
                        hold_reason: delivery.hold_reason.clone(),
                    },
                )
                .collect(),
            peers,
            working_memory: self.role_memory_paths(&caller_terminal),
        };
        encode_success(id, ResponseResult::RoleStatus { status })
    }

    pub(super) fn handle_overseer_escalate(
        &mut self,
        id: String,
        params: OverseerEscalateParams,
    ) -> String {
        let Some(caller) = self.parse_reporting_pane_id(&params.caller_pane_id) else {
            return encode_error(id, "pane_not_found", "calling pane not found");
        };
        if params
            .delivery_key
            .as_deref()
            .is_some_and(|key| !Self::validate_delivery_key(key))
        {
            return encode_error(
                id,
                "invalid_delivery_key",
                "delivery key must be 1 to 128 ASCII letters, digits, or - _ . : characters",
            );
        }
        let sender = self.state.terminal_id_for_any_pane(caller);
        let receivers = self
            .state
            .terminals
            .iter()
            .filter(|(_, terminal)| terminal.overseer.is_session_overlord())
            .map(|(terminal_id, _)| terminal_id.clone())
            .collect::<Vec<_>>();
        let binding = sender.as_ref().and_then(|sender| {
            self.delivery_binding_sha256(
                sender,
                Kind::Space,
                &receivers,
                Kind::Chief,
                "escalation",
                &params.text,
                params.wake,
            )
        });
        if let Some(response) =
            self.keyed_delivery_retry(&id, params.delivery_key.as_deref(), binding.clone())
        {
            return response;
        }
        let previous = self.overseer_service.mail_seq;
        match self.overseer_escalate(caller, &params.text, params.wake, std::time::Instant::now()) {
            Ok(count) => {
                let message_ids = (1..=count)
                    .map(|offset| self.overseer_service.message_id(previous + offset as u64))
                    .collect::<Vec<_>>();
                if let (Some(key), Some(binding)) = (params.delivery_key, binding) {
                    self.remember_logical_delivery(key, binding, message_ids.clone());
                }
                self.delivery_receipt(id, message_ids)
            }
            Err((code, why)) => encode_error(id, code, why),
        }
    }

    pub(super) fn handle_overseer_message(
        &mut self,
        id: String,
        params: OverseerMessageParams,
    ) -> String {
        let Some(caller) = self.parse_reporting_pane_id(&params.caller_pane_id) else {
            return encode_error(id, "pane_not_found", "calling pane not found");
        };
        let Some((_, target)) = self.parse_pane_id(&params.to) else {
            return encode_error(id, "pane_not_found", "target pane not found");
        };
        if params
            .delivery_key
            .as_deref()
            .is_some_and(|key| !Self::validate_delivery_key(key))
        {
            return encode_error(
                id,
                "invalid_delivery_key",
                "delivery key must be 1 to 128 ASCII letters, digits, or - _ . : characters",
            );
        }
        let sender = self.state.terminal_id_for_any_pane(caller);
        let receiver = self.state.terminal_id_for_any_pane(target);
        let binding = sender
            .as_ref()
            .zip(receiver.as_ref())
            .and_then(|(sender, receiver)| {
                self.delivery_binding_sha256(
                    sender,
                    Kind::Chief,
                    std::slice::from_ref(receiver),
                    Kind::Space,
                    "downward",
                    &params.text,
                    true,
                )
            });
        if let Some(response) =
            self.keyed_delivery_retry(&id, params.delivery_key.as_deref(), binding.clone())
        {
            return response;
        }
        let previous = self.overseer_service.mail_seq;
        match self.overseer_message(caller, target, &params.text, std::time::Instant::now()) {
            Ok(()) => {
                let message_id = self.overseer_service.message_id(previous + 1);
                if let (Some(key), Some(binding)) = (params.delivery_key, binding) {
                    self.remember_logical_delivery(key, binding, vec![message_id.clone()]);
                }
                self.delivery_receipt(id, vec![message_id])
            }
            Err((code, why)) => encode_error(id, code, why),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::TerminalState;

    #[test]
    fn role_status_reports_caller_revision_and_missing_space_holder() {
        let (_, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state = crate::app::state::AppState::test_new();
        let first = crate::workspace::Workspace::test_new("First");
        let first_pane = first.tabs[0].root_pane;
        let first_id = first.terminal_id(first_pane).unwrap().clone();
        app.state.terminals.insert(
            first_id.clone(),
            TerminalState::new(first_id.clone(), std::env::temp_dir()),
        );
        app.state.workspaces.push(first);
        let second = crate::workspace::Workspace::test_new("Second");
        let second_pane = second.tabs[0].root_pane;
        let second_id = second.terminal_id(second_pane).unwrap().clone();
        app.state.terminals.insert(
            second_id.clone(),
            TerminalState::new(second_id.clone(), std::env::temp_dir()),
        );
        app.state.workspaces.push(second);
        assert!(app.state.set_space_overseer(&first_id, true));

        let caller_pane_id = app.public_pane_id(0, first_pane).unwrap();
        let inspected_pane_id = app.public_pane_id(1, second_pane).unwrap();
        let response: serde_json::Value = serde_json::from_str(&app.handle_role_status(
            "status".into(),
            RoleStatusParams {
                caller_pane_id: caller_pane_id.clone(),
                inspected_pane_id: Some(inspected_pane_id),
            },
        ))
        .unwrap();
        let status = &response["result"]["status"];
        assert_eq!(status["caller_overseer"], true);
        assert_eq!(status["caller_overlord"], false);
        assert_eq!(status["role_revision"], 1);
        assert_eq!(
            status["caller_identity_source"],
            "explicit pane identifier resolved to live terminal"
        );
        assert!(status["caller_raw_pane_id"]
            .as_str()
            .unwrap()
            .starts_with("p_"));
        assert_eq!(status["caller_public_pane_id"], caller_pane_id);
        assert!(status["caller_native_session"].is_null());
        assert_eq!(status["caller_ui_focused"], false);
        assert_eq!(status["space_overseers"].as_array().unwrap().len(), 2);
        assert!(status["space_overseers"][1]["overseer"].is_null());
        assert_eq!(status["schema_version"], 1);
        assert_eq!(status["peers"].as_array().unwrap().len(), 2);
        let caller_peer = status["peers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|peer| peer["terminal_id"] == first_id.to_string())
            .unwrap();
        assert!(caller_peer["raw_pane_id"]
            .as_str()
            .unwrap()
            .starts_with("p_"));
        assert_eq!(caller_peer["pane_id"], caller_pane_id);
        assert_eq!(status["inspected_terminal_id"], second_id.to_string());
        assert!(status["inspected_raw_pane_id"]
            .as_str()
            .unwrap()
            .starts_with("p_"));
    }

    #[test]
    fn role_status_reports_native_conversation_without_using_focus_as_caller_identity() {
        let (_, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state = crate::app::state::AppState::test_new();
        let workspace = crate::workspace::Workspace::test_new("First");
        let caller = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(caller).unwrap().clone();
        app.state.workspaces.push(workspace);
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
                source: "herdr:codex".into(),
                agent: "codex".into(),
                session_ref: crate::agent_resume::AgentSessionRef::id("native-conversation")
                    .unwrap(),
            });
        let caller_pane_id = app.public_pane_id(0, caller).unwrap();
        let response: serde_json::Value = serde_json::from_str(&app.handle_role_status(
            "status".into(),
            RoleStatusParams {
                caller_pane_id,
                inspected_pane_id: None,
            },
        ))
        .unwrap();
        let status = &response["result"]["status"];
        assert_eq!(status["caller_terminal_id"], terminal_id.to_string());
        assert_eq!(
            status["caller_native_session"]["value"],
            "native-conversation"
        );
        assert_eq!(status["caller_ui_focused"], true);
        app.state.active = None;
        let response: serde_json::Value = serde_json::from_str(&app.handle_role_status(
            "status-unfocused".into(),
            RoleStatusParams {
                caller_pane_id: app.public_pane_id(0, caller).unwrap(),
                inspected_pane_id: None,
            },
        ))
        .unwrap();
        let status = &response["result"]["status"];
        assert_eq!(
            status["caller_native_session"]["value"],
            "native-conversation"
        );
        assert_eq!(status["caller_ui_focused"], false);

        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .persisted_agent_session = None;
        let response: serde_json::Value = serde_json::from_str(&app.handle_role_status(
            "status-rotated".into(),
            RoleStatusParams {
                caller_pane_id: app.public_pane_id(0, caller).unwrap(),
                inspected_pane_id: None,
            },
        ))
        .unwrap();
        assert!(response["result"]["status"]["caller_native_session"].is_null());
    }
}

//! `pane.restart_agent`: quit a pane's agent with its own quit command, wait
//! for the pane's shell to come back, then start the pane's pinned launch
//! command there.
//!
//! This is the same shape as a resume (a command typed into the pane's shell),
//! driven by the pin instead of a saved session. It refuses a pane that is
//! working or blocked, has no pin, or whose pin names no launcher, so a
//! restart never cuts off work or guesses at a program.

use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::api::schema::{PaneTarget, ResponseResult};

use super::api::responses::{encode_error, encode_success};
use super::api_helpers::{deliver_text_then_keys, encode_api_keys, encode_api_text};
use super::App;

/// The shell must stay free this long before the launch command is typed, so
/// a quitting agent that is only redrawing does not get a second command.
const SHELL_FREE_SETTLE: Duration = Duration::from_millis(400);
/// A restart that has not reached the launch by now is abandoned and the
/// pane is left as it is.
const RESTART_TIMEOUT: Duration = Duration::from_secs(30);
const RESTART_POLL: Duration = Duration::from_millis(250);

/// A restart in progress on one terminal.
#[derive(Debug, Clone)]
pub struct AgentRestart {
    pub argv: Vec<String>,
    pub started: Instant,
    pub shell_free_since: Option<Instant>,
}

/// The agent's own command for ending the session cleanly.
pub(crate) fn quit_command(agent_label: &str) -> Option<&'static str> {
    match agent_label {
        "codex" => Some("/quit"),
        "claude" => Some("/exit"),
        "cursor" => Some("/quit"),
        _ => None,
    }
}

impl App {
    pub(super) fn handle_pane_restart_agent(&mut self, id: String, target: PaneTarget) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&target.pane_id) else {
            return encode_error(
                id,
                "pane_not_found",
                format!("pane {} not found", target.pane_id),
            );
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.terminal_id(pane_id))
            .cloned()
        else {
            return encode_error(id, "pane_not_found", "pane has no terminal".to_string());
        };
        let Some(terminal) = self.state.terminals.get(&terminal_id) else {
            return encode_error(id, "pane_not_found", "pane has no terminal".to_string());
        };
        let Some(pin) = terminal.model_pin.as_ref() else {
            return encode_error(
                id,
                "no_model_pin",
                "pane has no pin; run `herdr pane pin-model` first".to_string(),
            );
        };
        let Some(argv) = pin.launch_argv() else {
            return encode_error(
                id,
                "no_launcher",
                "the pin names no launcher; pin again with --launcher".to_string(),
            );
        };
        if terminal.pending_restart.is_some() {
            return encode_error(
                id,
                "restart_in_progress",
                "this pane is already restarting".to_string(),
            );
        }
        if matches!(
            terminal.state,
            crate::detect::AgentState::Working | crate::detect::AgentState::Blocked
        ) {
            return encode_error(
                id,
                "agent_busy",
                "the agent is working or blocked; restart when it is idle".to_string(),
            );
        }
        let agent_label = terminal.effective_agent_label().map(str::to_string);
        let Some(runtime) = self.terminal_runtimes.get(&terminal_id) else {
            return encode_error(id, "no_runtime", "the pane is not running".to_string());
        };
        let agent_running = runtime.foreground_command().is_some();
        if agent_running {
            let Some(quit) = agent_label.as_deref().and_then(quit_command) else {
                return encode_error(
                    id,
                    "no_quit_command",
                    "herdr does not know this agent's quit command".to_string(),
                );
            };
            let Ok(enter) = encode_api_keys(runtime, &["Enter".into()]) else {
                return encode_error(id, "invalid_key", "cannot encode Enter".to_string());
            };
            let enter: Vec<u8> = enter.into_iter().flatten().collect();
            let text = encode_api_text(runtime, quit);
            if let Err((code, message)) = deliver_text_then_keys(runtime, &text, &enter) {
                return encode_error(id, code, message);
            }
        }
        let now = Instant::now();
        if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
            terminal.pending_restart = Some(AgentRestart {
                argv,
                started: now,
                shell_free_since: (!agent_running).then_some(now),
            });
        }
        let pane = self.pane_info(ws_idx, pane_id).unwrap();
        encode_success(id, ResponseResult::PaneInfo { pane })
    }

    /// Whether any restart is waiting on its shell, and so needs a tick.
    pub(crate) fn agent_restart_deadline(&self) -> Option<Instant> {
        self.state
            .terminals
            .values()
            .any(|terminal| terminal.pending_restart.is_some())
            .then(|| Instant::now() + RESTART_POLL)
    }

    /// Move each restart on: once the quit has returned the pane to its
    /// shell and the shell has stayed free, type the pinned launch command.
    pub(crate) fn advance_agent_restarts(&mut self, now: Instant) -> bool {
        let waiting: Vec<_> = self
            .state
            .terminals
            .iter()
            .filter(|(_, terminal)| terminal.pending_restart.is_some())
            .map(|(id, _)| id.clone())
            .collect();
        let mut changed = false;
        for terminal_id in waiting {
            let Some(restart) = self
                .state
                .terminals
                .get(&terminal_id)
                .and_then(|terminal| terminal.pending_restart.clone())
            else {
                continue;
            };
            let abandon = |app: &mut Self, reason: &str| {
                tracing::warn!(terminal = %terminal_id, reason, "agent restart abandoned");
                if let Some(terminal) = app.state.terminals.get_mut(&terminal_id) {
                    terminal.pending_restart = None;
                }
            };
            if now.duration_since(restart.started) > RESTART_TIMEOUT {
                abandon(self, "the agent did not leave the pane in time");
                changed = true;
                continue;
            }
            let Some(runtime) = self.terminal_runtimes.get(&terminal_id) else {
                abandon(self, "the pane is gone");
                changed = true;
                continue;
            };
            if runtime.foreground_command().is_some() {
                if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                    if let Some(pending) = terminal.pending_restart.as_mut() {
                        pending.shell_free_since = None;
                    }
                }
                continue;
            }
            let free_since = restart.shell_free_since.unwrap_or(now);
            if now.duration_since(free_since) < SHELL_FREE_SETTLE {
                if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                    if let Some(pending) = terminal.pending_restart.as_mut() {
                        pending.shell_free_since = Some(free_since);
                    }
                }
                continue;
            }
            let Some(mut command) = super::agent_resume::shell_command_from_argv(&restart.argv)
            else {
                abandon(self, "empty launch command");
                changed = true;
                continue;
            };
            command.push('\r');
            if runtime.try_send_bytes(Bytes::from(command)).is_err() {
                abandon(self, "could not type the launch command");
                changed = true;
                continue;
            }
            if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                terminal.pending_restart = None;
                // The old agent is gone; the new one reports itself afresh.
                terminal.clear_agent_runtime_identity_after_respawn();
            }
            self.state.mark_session_dirty();
            changed = true;
        }
        changed
    }
}

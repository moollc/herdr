use super::*;
use crate::api::schema::{
    AgentPromptParams, AgentReadParams, AgentSendParams, AgentTarget, EmptyParams, Method,
    PaneTarget, ReadFormat, ReadSource, Request,
};
use crate::config::Config;
use crate::detect::{Agent, AgentState};
use crate::layout::PaneId;
use crate::terminal::TerminalId;
use crate::workspace::Workspace;
use serde_json::Value;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

struct Fixture {
    app: App,
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("a"), Workspace::test_new("b")];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        let root = std::env::temp_dir().join(format!("agent-close-{}", TerminalId::alloc()));
        app.overseer_service.situation_path = root.join("overseer/situation.md");
        app.overseer_service.tasks_path = None;
        Self { app, root }
    }
    fn agent(&mut self, ws: usize, idle: u64) -> PaneId {
        let pane = self.app.state.workspaces[ws].test_split(ratatui::layout::Direction::Horizontal);
        self.app.state.ensure_test_terminals();
        let terminal = self.terminal(pane);
        let state = self.app.state.terminals.get_mut(&terminal).unwrap();
        state.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        state.agent_last_finished_at = Some(SystemTime::now() - Duration::from_secs(idle));
        pane
    }
    fn terminal(&self, pane: PaneId) -> TerminalId {
        self.app.state.terminal_id_for_any_pane(pane).unwrap()
    }
    fn detach(&mut self, pane: PaneId) {
        let ws = self.app.state.workspace_index_for_pane(pane).unwrap();
        let public = self.app.public_pane_id(ws, pane).unwrap();
        let response = self
            .app
            .handle_pane_close("detach".into(), PaneTarget { pane_id: public });
        assert!(json(&response).get("error").is_none(), "{response}");
        assert!(self.app.state.is_detached_agent_pane(pane));
    }
    fn role(&mut self, pane: PaneId, chief: bool) {
        let id = self.terminal(pane);
        let role = &mut self.app.state.terminals.get_mut(&id).unwrap().overseer;
        role.session_overlord = chief;
        role.space_overseer = !chief;
    }
    fn close(&mut self, params: AgentCloseParams) -> Value {
        json(&self.app.handle_api_request(Request {
            id: "close".into(),
            method: Method::AgentClose(params),
        }))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Only this test's synthetic files and throwaway PTYs are owned here.
        for (_, runtime) in self.app.terminal_runtimes.drain() {
            runtime.terminate_agent_processes();
            runtime.shutdown();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}
fn raw(pane: PaneId) -> String {
    format!("p_{}", pane.raw())
}
fn bulk() -> AgentCloseParams {
    AgentCloseParams {
        detached: true,
        ..Default::default()
    }
}
fn target(pane: PaneId) -> AgentCloseParams {
    AgentCloseParams {
        target: Some(raw(pane)),
        apply: true,
        ..Default::default()
    }
}
fn code(value: &Value) -> &str {
    value["error"]["code"].as_str().unwrap_or("")
}

#[test]
fn detached_visibility_raw_resolution_and_default_preview_do_not_mutate() {
    let mut f = Fixture::new();
    let pane = f.agent(0, 900);
    let terminal = f.terminal(pane);
    f.detach(pane);
    let list = json(&f.app.handle_api_request(Request {
        id: "list".into(),
        method: Method::AgentList(EmptyParams::default()),
    }));
    let info = &list["result"]["agents"][0];
    assert_eq!(info["workspace_id"], "detached");
    assert_eq!(info["tab_id"], "detached");
    assert_eq!(info["pane_id"], raw(pane));
    for name in [raw(pane), terminal.to_string()] {
        let get = json(
            &f.app
                .handle_agent_get("get".into(), AgentTarget { target: name }),
        );
        assert_eq!(get["result"]["agent"]["pane_id"], raw(pane));
    }
    let response = f.close(bulk());
    assert_eq!(response["result"]["type"], "agent_close");
    assert_eq!(response["result"]["dry_run"], true);
    assert_eq!(response["result"]["closed_count"], 0);
    assert_eq!(response["result"]["would_close_count"], 1);
    assert_eq!(f.app.state.detached_agents.len(), 1);
    assert!(f.app.state.terminals.contains_key(&terminal));
    assert!(!f.root.exists());
}

#[test]
fn idle_threshold_and_missing_timing_are_reported() {
    let mut f = Fixture::new();
    let short = f.agent(0, 120);
    let long = f.agent(0, 900);
    let unknown = f.agent(0, 900);
    let id = f.terminal(unknown);
    f.app
        .state
        .terminals
        .get_mut(&id)
        .unwrap()
        .agent_last_finished_at = None;
    for pane in [short, long, unknown] {
        f.detach(pane);
    }
    let response = f.close(AgentCloseParams {
        idle_over_seconds: Some(300),
        apply: true,
        ..bulk()
    });
    assert_eq!(response["result"]["closed_count"], 1, "{response}");
    assert_eq!(response["result"]["skipped_count"], 2);
    assert!(f.app.state.is_detached_agent_pane(short));
    assert!(!f.app.state.is_detached_agent_pane(long));
    for c in response["result"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["action"] == "skipped")
    {
        assert_eq!(c["reason"], "idle_under_threshold");
    }
}

#[test]
fn working_and_blocked_need_force_but_force_never_bypasses_idle_filter() {
    for (state, reason) in [
        (AgentState::Working, "agent_working"),
        (AgentState::Blocked, "agent_blocked"),
    ] {
        let mut f = Fixture::new();
        let pane = f.agent(0, 900);
        f.detach(pane);
        let id = f.terminal(pane);
        f.app.state.terminals.get_mut(&id).unwrap().state = state;
        let preview = f.close(bulk());
        assert_eq!(preview["result"]["candidates"][0]["reason"], reason);
        let blocked = f.close(AgentCloseParams {
            apply: true,
            ..bulk()
        });
        assert_eq!(blocked["result"]["candidates"][0]["reason"], reason);
        assert_eq!(code(&f.close(target(pane))), "agent_busy");
        let filtered = f.close(AgentCloseParams {
            force: true,
            idle_over_seconds: Some(0),
            apply: true,
            ..bulk()
        });
        assert_eq!(
            filtered["result"]["candidates"][0]["reason"],
            "idle_under_threshold"
        );
        assert_eq!(
            f.close(AgentCloseParams {
                force: true,
                apply: true,
                ..bulk()
            })["result"]["closed_count"],
            1
        );
    }
}

#[test]
fn caller_and_active_roles_remain_protected_even_with_force() {
    for chief in [false, true] {
        let mut f = Fixture::new();
        let caller = f.agent(0, 900);
        f.role(caller, chief);
        assert_eq!(
            code(&f.close(AgentCloseParams {
                caller_pane_id: Some(raw(caller)),
                force: true,
                ..target(caller)
            })),
            "cannot_close_caller"
        );
        let holder = f.agent(0, 900);
        f.role(holder, chief);
        assert_eq!(
            code(&f.close(AgentCloseParams {
                force: true,
                ..target(holder)
            })),
            "active_overseer_or_overlord"
        );
        f.detach(holder);
        let response = f.close(AgentCloseParams {
            force: true,
            apply: true,
            ..bulk()
        });
        assert_eq!(
            response["result"]["candidates"][0]["reason"],
            "active_overseer_or_overlord"
        );
        assert!(f.app.state.is_detached_agent_pane(holder));
    }
}

#[test]
fn space_overseer_scope_and_explicit_detached_override() {
    let mut f = Fixture::new();
    let caller = f.agent(0, 900);
    f.role(caller, false);
    let own = f.agent(0, 900);
    let other = f.agent(1, 900);
    let detached = f.agent(1, 900);
    f.detach(detached);
    let caller_id = Some(raw(caller));
    assert_eq!(
        code(&f.close(AgentCloseParams {
            caller_pane_id: caller_id.clone(),
            ..target(other)
        })),
        "permission_denied"
    );
    assert_eq!(
        code(&f.close(AgentCloseParams {
            caller_pane_id: caller_id.clone(),
            ..target(detached)
        })),
        "permission_denied"
    );
    assert_eq!(
        code(&f.close(AgentCloseParams {
            caller_pane_id: caller_id.clone(),
            ..bulk()
        })),
        "permission_denied"
    );
    let own_result = f.close(AgentCloseParams {
        caller_pane_id: caller_id.clone(),
        ..target(own)
    });
    assert_eq!(own_result["result"]["closed_count"], 1, "{own_result}");
    assert!(!f.app.state.is_detached_agent_pane(own));
    assert_eq!(
        f.close(AgentCloseParams {
            caller_pane_id: caller_id,
            allow_detached: true,
            apply: true,
            ..bulk()
        })["result"]["closed_count"],
        1
    );
    assert!(f.app.state.terminal_id_for_any_pane(other).is_some());
}

#[test]
fn overlord_and_external_operator_cover_other_spaces_and_detached() {
    for external in [false, true] {
        let mut f = Fixture::new();
        let caller = f.agent(0, 900);
        f.role(caller, true);
        let other = f.agent(1, 900);
        let detached = f.agent(1, 900);
        f.detach(detached);
        let caller_id = (!external).then(|| raw(caller));
        assert_eq!(
            f.close(AgentCloseParams {
                caller_pane_id: caller_id.clone(),
                ..target(other)
            })["result"]["closed_count"],
            1
        );
        assert_eq!(
            f.close(AgentCloseParams {
                caller_pane_id: caller_id,
                apply: true,
                ..bulk()
            })["result"]["closed_count"],
            1
        );
    }
}

#[test]
fn workers_and_invalid_callers_fail_closed_for_every_scope() {
    let mut f = Fixture::new();
    let caller = f.agent(0, 900);
    let own = f.agent(0, 900);
    let other = f.agent(1, 900);
    let detached = f.agent(1, 900);
    f.detach(detached);
    for caller_id in [raw(caller), "p_999999999".into(), "".into()] {
        for params in [target(own), target(other), target(detached), bulk()] {
            assert_eq!(
                code(&f.close(AgentCloseParams {
                    caller_pane_id: Some(caller_id.clone()),
                    allow_detached: true,
                    force: true,
                    ..params
                })),
                "permission_denied"
            );
        }
    }
    assert_eq!(f.app.state.detached_agents.len(), 1);
}

#[test]
fn invalid_selection_ambiguity_and_detached_assertion_are_safe() {
    let mut f = Fixture::new();
    assert_eq!(
        code(&f.close(AgentCloseParams::default())),
        "invalid_params"
    );
    assert_eq!(f.close(bulk())["result"]["would_close_count"], 0);
    let a = f.agent(0, 900);
    let b = f.agent(0, 900);
    let asserted = f.close(AgentCloseParams {
        detached: true,
        ..target(a)
    });
    assert_eq!(
        asserted["result"]["candidates"][0]["reason"],
        "not_detached"
    );
    f.detach(a);
    f.detach(b);
    let ambiguous = f.close(AgentCloseParams {
        target: Some("pi".into()),
        ..Default::default()
    });
    assert_eq!(code(&ambiguous), "agent_target_ambiguous");
    let resolved = f.app.resolve_terminal_target("pi").unwrap_err();
    assert!(
        matches!(resolved, crate::app::terminal_targets::TerminalTargetError::Ambiguous { candidates, .. } if candidates.len() == 2)
    );
    assert_eq!(f.app.state.detached_agents.len(), 2);
}

#[test]
fn apply_persists_audit_and_refreshes_situation_and_space_memory() {
    let mut f = Fixture::new();
    let docked = f.agent(0, 900);
    let workspace = f.app.state.workspaces[0].id.clone();
    let detached = f.agent(1, 900);
    let detached_terminal = f.terminal(detached).to_string();
    f.detach(detached);
    assert_eq!(f.close(target(docked))["result"]["closed_count"], 1);
    assert_eq!(
        f.close(AgentCloseParams {
            apply: true,
            ..bulk()
        })["result"]["closed_count"],
        1
    );
    let situation = std::fs::read_to_string(&f.app.overseer_service.situation_path).unwrap();
    let detached_section = situation
        .split("## Detached")
        .nth(1)
        .unwrap()
        .split("## Agent close audit")
        .next()
        .unwrap();
    assert!(!detached_section.contains(&detached_terminal));
    assert!(situation.contains("agent.close caller=external closed"));
    let root = f
        .app
        .overseer_service
        .situation_path
        .parent()
        .unwrap()
        .join("role-memory");
    for key in ["session:overlord".to_string(), format!("space:{workspace}")] {
        let records =
            crate::role_continuity::read_records(&crate::role_continuity::memory_path(&root, &key))
                .unwrap();
        assert!(records.iter().any(|r| matches!(r, crate::role_continuity::ContinuityRecord::Memory { text, .. } if text.starts_with("agent.close "))));
    }
    // A subsequent refresh reads the persistent audit rather than overwriting it.
    f.app.log_agent_close_to_situation(None, &[]).unwrap();
    assert!(
        std::fs::read_to_string(&f.app.overseer_service.situation_path)
            .unwrap()
            .contains(&detached_terminal)
    );
}

#[tokio::test]
async fn detached_read_send_prompt_and_termination_use_the_owned_runtime() {
    let mut f = Fixture::new();
    let pane = f.agent(0, 900);
    let id = f.terminal(pane);
    let runtime = crate::terminal::TerminalRuntime::spawn_argv_command(
        pane,
        24,
        80,
        std::env::temp_dir(),
        &[
            "/bin/sh".into(),
            "-c".into(),
            "echo ready; exec /bin/cat".into(),
        ],
        1024 * 1024,
        f.app.state.host_terminal_theme,
        f.app.event_tx.clone(),
        f.app.render_notify.clone(),
        f.app.render_dirty.clone(),
    )
    .unwrap();
    let pid = runtime.child_pid();
    f.app.terminal_runtimes.insert(id.clone(), runtime);
    f.detach(pane);
    let get_read = json(&f.app.handle_agent_read(
        "read".into(),
        AgentReadParams {
            target: raw(pane),
            source: ReadSource::Recent,
            lines: Some(20),
            format: ReadFormat::Text,
            strip_ansi: true,
        },
    ));
    assert_eq!(
        get_read["result"]["read"]["workspace_id"], "detached",
        "{get_read}"
    );
    assert_eq!(get_read["result"]["read"]["tab_id"], "detached");
    assert_eq!(get_read["result"]["read"]["pane_id"], raw(pane));
    assert!(json(&f.app.handle_agent_send(
        "send".into(),
        AgentSendParams {
            target: raw(pane),
            text: "literal".into()
        }
    ))
    .get("error")
    .is_none());
    assert!(json(&f.app.handle_agent_prompt(
        "prompt".into(),
        AgentPromptParams {
            target: id.to_string(),
            text: "".into()
        }
    ))
    .get("error")
    .is_none());
    let preview = f.close(bulk());
    assert_eq!(preview["result"]["would_close_count"], 1);
    assert!(crate::platform::process_exists(pid));
    assert_eq!(
        f.close(AgentCloseParams {
            apply: true,
            ..bulk()
        })["result"]["closed_count"],
        1
    );
    assert!(f.app.terminal_runtimes.get(&id).is_none());
    assert!(!f.app.state.terminals.contains_key(&id));
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while crate::platform::process_exists(pid) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !crate::platform::process_exists(pid),
        "throwaway child survived close"
    );
}

#[test]
fn dry_run_dispatcher_keeps_a_detached_only_session_without_creating_a_workspace() {
    let mut f = Fixture::new();
    let pane = f.agent(0, 900);
    f.detach(pane);
    f.app.state.workspaces.clear();
    f.app.state.active = None;
    let (tx, rx) = std::sync::mpsc::channel();
    let changed = f
        .app
        .handle_api_request_message(crate::api::ApiRequestMessage {
            request: Request {
                id: "preview".into(),
                method: Method::AgentClose(bulk()),
            },
            respond_to: tx,
        });
    assert!(!changed);
    assert_eq!(json(&rx.recv().unwrap())["result"]["would_close_count"], 1);
    assert!(f.app.state.workspaces.is_empty());
    assert!(f.app.terminal_runtimes.values().next().is_none());
}

#[test]
fn a_single_detached_assertion_still_returns_busy_and_protection_errors() {
    let mut f = Fixture::new();
    let pane = f.agent(0, 900);
    f.detach(pane);
    let id = f.terminal(pane);
    f.app.state.terminals.get_mut(&id).unwrap().state = AgentState::Working;
    assert_eq!(
        code(&f.close(AgentCloseParams {
            detached: true,
            ..target(pane)
        })),
        "agent_busy"
    );
    f.role(pane, true);
    assert_eq!(
        code(&f.close(AgentCloseParams {
            detached: true,
            force: true,
            ..target(pane)
        })),
        "active_overseer_or_overlord"
    );
}

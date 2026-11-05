//! Startup hook and literal input regressions.
use super::tests::app_with_two_panes;
use super::*;

fn session_start(
    app: &mut App,
    pane: crate::layout::PaneId,
    session: &str,
    source: &str,
    seq: u64,
) -> String {
    session_start_as(app, pane, session, source, seq, "codex")
}

fn session_start_as(
    app: &mut App,
    pane: crate::layout::PaneId,
    session: &str,
    source: &str,
    seq: u64,
    agent: &str,
) -> String {
    app.handle_pane_report_agent_session(
        "startup".into(),
        PaneReportAgentSessionParams {
            turn_reporting: false,
            pane_id: format!("p_{}", pane.raw()),
            source: format!("herdr:{agent}"),
            agent: agent.into(),
            startup_source: Some(source.into()),
            seq: Some(seq),
            agent_session_id: Some(session.into()),
            agent_session_path: None,
        },
    )
}

fn acknowledge(app: &mut App, pane: crate::layout::PaneId, session: &str, offer: &str) -> String {
    acknowledge_as(app, pane, session, offer, "codex")
}

fn acknowledge_as(
    app: &mut App,
    pane: crate::layout::PaneId,
    session: &str,
    offer: &str,
    agent: &str,
) -> String {
    let response: serde_json::Value = serde_json::from_str(offer).unwrap();
    let nonce = response["result"]["nonce"].as_str().unwrap();
    app.handle_pane_ack_orientation(
        "ack".into(),
        PaneAckOrientationParams {
            pane_id: format!("p_{}", pane.raw()),
            source: format!("herdr:{agent}"),
            agent_session_id: session.into(),
            nonce: nonce.into(),
        },
    )
}

async fn submitted_bytes(rx: &mut tokio::sync::mpsc::Receiver<bytes::Bytes>) -> Vec<u8> {
    let mut got = rx.try_recv().expect("prompt text").to_vec();
    if !got.ends_with(b"\r") {
        let enter = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("submit enter")
            .expect("submit channel open");
        got.extend_from_slice(&enter);
    }
    got
}

#[tokio::test]
async fn ordinary_api_submission_closes_only_a_pending_notification_origin() {
    let (mut app, pane) = app_with_two_panes();
    let terminal_id = app.state.terminal_id_for_any_pane(pane).unwrap();
    app.state
        .terminals
        .get_mut(&terminal_id)
        .unwrap()
        .set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
            source: "herdr:pi".into(),
            agent: "pi".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id("api-session").unwrap(),
        });
    let mut notification = app
        .message_origin_for_api(pane, Some("notification"), Some("api-notice"), None)
        .unwrap()
        .unwrap();
    notification.active = true;
    app.store_message_origin(pane, Some(notification));
    let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
    app.terminal_runtimes.insert(terminal_id.clone(), runtime);
    let response = app.handle_pane_send_input(
        "ordinary".into(),
        PaneSendInputParams {
            pane_id: app.public_pane_id(0, pane).unwrap(),
            text: "follow up".into(),
            keys: vec!["Enter".into()],
            origin: None,
            correlation_id: None,
            task_id: None,
        },
    );
    assert!(response.contains("result"), "{response}");
    assert_eq!(submitted_bytes(&mut rx).await, b"follow up\r");
    assert!(app.state.terminals[&terminal_id].message_origin.is_none());

    let response = app.handle_pane_send_input(
        "checked-task".into(),
        PaneSendInputParams {
            pane_id: app.public_pane_id(0, pane).unwrap(),
            text: "checked task".into(),
            keys: vec!["Enter".into()],
            origin: Some("task".into()),
            correlation_id: Some("api-task-correlation".into()),
            task_id: Some("api-task-9".into()),
        },
    );
    assert!(response.contains("result"), "{response}");
    assert_eq!(submitted_bytes(&mut rx).await, b"checked task\r");
    let origin = app.state.terminals[&terminal_id]
        .message_origin
        .as_ref()
        .unwrap();
    assert_eq!(origin.correlation_id, "api-task-correlation");
    assert_eq!(origin.task_id.as_deref(), Some("api-task-9"));
}

#[tokio::test]
#[ignore = "requires a separately provisioned compatible Codex client; set HERDR_TEST_CODEX and run explicitly"]
async fn external_codex_deepseek_model_request_receives_orientation_before_task() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::process::Command;
    let (mut app, pane) = app_with_two_panes();
    let terminal_id = app.state.terminal_id_for_any_pane(pane).unwrap();
    app.state
        .terminals
        .get_mut(&terminal_id)
        .unwrap()
        .detected_agent = Some(crate::detect::Agent::Codex);
    let name = crate::pane_names::assigned_names(&app.state.terminals)[&terminal_id].clone();
    let root = std::env::temp_dir().join(format!("herdr-real-codex-{}", pane.raw()));
    std::fs::create_dir_all(&root).unwrap();
    app.state.terminals.get_mut(&terminal_id).unwrap().cwd = root.join("project");
    let socket_path = root.join("hook.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let task = "  exact external panel task\nwith 'quotes' and --flags  \n";
    let mut child = Command::new("python3")
        .arg(repo.join("tests/fixtures/codex_orientation_client.py"))
        .arg(&root)
        .arg(repo.join("src/integration/assets/codex/herdr-agent-state.sh"))
        .arg(&socket_path)
        .arg(format!("p_{}", pane.raw()))
        .arg(task)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let (mut stream, _) = loop {
        match listener.accept() {
            Ok(stream) => break stream,
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(err) => {
                child.kill().ok();
                let output = child.wait_with_output().unwrap();
                panic!(
                    "startup hook did not connect: {err}; {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    };
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let mut request = String::new();
    BufReader::new(stream.try_clone().unwrap())
        .read_line(&mut request)
        .unwrap();
    writeln!(
        stream,
        "{}",
        app.handle_api_request(serde_json::from_str(&request).unwrap())
    )
    .unwrap();
    let (mut ack_stream, _) = loop {
        match listener.accept() {
            Ok(stream) => break stream,
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(err) => panic!("startup hook did not acknowledge orientation: {err}"),
        }
    };
    let mut ack = String::new();
    BufReader::new(ack_stream.try_clone().unwrap())
        .read_line(&mut ack)
        .unwrap();
    writeln!(
        ack_stream,
        "{}",
        app.handle_api_request(serde_json::from_str(&ack).unwrap())
    )
    .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let request: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let input = request["input"].as_array().unwrap();
    let orientation = input
        .iter()
        .position(|item| item.to_string().contains("Managed by Herdr"))
        .unwrap();
    let user = input
        .iter()
        .position(|item| {
            item["content"].as_array().is_some_and(|content| {
                content
                    .iter()
                    .any(|part| part["text"].as_str() == Some(task))
            })
        })
        .unwrap();
    assert!(
        orientation < user,
        "startup context precedes the unchanged user task"
    );
    let context = input[orientation].to_string();
    assert!(context.contains(&name));
    assert!(context.contains(&terminal_id.to_string()));
    for field in [
        "fork=",
        "version=",
        "workspace=",
        "pane address=",
        "Cwd=",
        "repo=",
        "branch=",
        "worktree=",
        "Read Herdr skill:",
        "Code index:",
        "AGENTS.md",
        "CLAUDE.md",
        "GEMINI.md",
        "docs/CODE_INDEX.md",
        "Discover peers first",
    ] {
        assert!(
            context.contains(field),
            "missing model-visible orientation field: {field}"
        );
    }
    assert!(context.contains(
        &root
            .join("project/SKILL.md")
            .canonicalize()
            .unwrap()
            .display()
            .to_string()
    ));
    assert_eq!(
        input
            .iter()
            .filter(|item| item.to_string().contains("Managed by Herdr"))
            .count(),
        1
    );
    assert_eq!(
        app.state.terminals[&terminal_id].orientation,
        crate::agent_orientation::OrientationState::Delivered
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn external_codex_deepseek_panel_hook_delivers_generated_identity_before_literal_task() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::process::{Command, Stdio};
    let (mut app, pane) = app_with_two_panes();
    let terminal_id = app.state.terminal_id_for_any_pane(pane).unwrap();
    let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
    terminal.detected_agent = Some(crate::detect::Agent::Codex);
    terminal.orientation = crate::agent_orientation::OrientationState::Suppressed;
    let expected_name =
        crate::pane_names::assigned_names(&app.state.terminals)[&terminal_id].clone();
    let socket_path = std::env::temp_dir().join(format!("herdr-hook-{}.sock", pane.raw()));
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).unwrap();
    let hook = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/integration/assets/codex/herdr-agent-state.sh");
    let child = std::thread::spawn(move || {
        let mut child = Command::new("sh")
            .arg(hook)
            .arg("session")
            .env("HERDR_ENV", "1")
            .env("HERDR_SOCKET_PATH", &socket_path)
            .env("HERDR_PANE_ID", format!("p_{}", pane.raw()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(br#"{"hook_event_name":"SessionStart","session_id":"external-codex","source":"startup","model":"DeepSeek V4 Flash"}"#).unwrap();
        child.wait_with_output().unwrap()
    });
    let (mut stream, _) = listener.accept().unwrap();
    let mut request = String::new();
    BufReader::new(stream.try_clone().unwrap())
        .read_line(&mut request)
        .unwrap();
    let request: crate::api::schema::Request = serde_json::from_str(&request).unwrap();
    let response = app.handle_api_request(request);
    writeln!(stream, "{response}").unwrap();
    let (mut ack_stream, _) = listener.accept().unwrap();
    let mut ack = String::new();
    BufReader::new(ack_stream.try_clone().unwrap())
        .read_line(&mut ack)
        .unwrap();
    writeln!(
        ack_stream,
        "{}",
        app.handle_api_request(serde_json::from_str(&ack).unwrap())
    )
    .unwrap();
    let output = child.join().unwrap();
    assert!(output.status.success());
    let output: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let context = output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert_eq!(
        output["hookSpecificOutput"]["hookEventName"],
        "SessionStart"
    );
    assert!(context.contains("Managed by Herdr"));
    assert!(context.contains(&format!("pane name=\"{expected_name}\"")));
    assert!(context.contains(&terminal_id.to_string()));
    assert!(context.contains(&app.public_pane_id(0, pane).unwrap()));
    assert!(
        !context.contains("DeepSeek"),
        "backend branding is not harness evidence"
    );
    assert_eq!(
        app.state.terminals[&terminal_id].orientation,
        crate::agent_orientation::OrientationState::Delivered
    );
    let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
    app.terminal_runtimes.insert(terminal_id.clone(), runtime);
    // Panel task transport stays literal; the hook has already supplied context.
    let task = "first\n  exact task --flags 'quotes'\n";
    let response = app.handle_pane_send_input(
        "task".into(),
        PaneSendInputParams {
            pane_id: app.public_pane_id(0, pane).unwrap(),
            text: task.into(),
            keys: vec!["Enter".into()],
            origin: None,
            correlation_id: None,
            task_id: None,
        },
    );
    assert!(response.contains("result"));
    assert_eq!(
        submitted_bytes(&mut rx).await,
        format!("{task}\r").as_bytes()
    );
    assert!(
        !session_start(&mut app, pane, "external-codex", "startup", u64::MAX - 2)
            .contains("context")
    );
    assert!(
        session_start(&mut app, pane, "external-codex", "resume", u64::MAX - 1).contains("context")
    );
    // Same surviving terminal, new process and verified fresh generation.
    app.state
        .terminals
        .get_mut(&terminal_id)
        .unwrap()
        .clear_agent_runtime_identity_after_respawn();
    assert!(
        session_start(&mut app, pane, "replacement", "startup", u64::MAX)
            .contains("Managed by Herdr")
    );
    std::fs::remove_file(std::env::temp_dir().join(format!("herdr-hook-{}.sock", pane.raw())))
        .unwrap();
}

#[tokio::test]
async fn orientation_hook_coordinates_structured_delivery_and_new_generation() {
    let (mut app, pane) = app_with_two_panes();
    let terminal_id = app.state.terminal_id_for_any_pane(pane).unwrap();
    let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
    terminal.detected_agent = Some(crate::detect::Agent::Codex);
    terminal.orientation = crate::agent_orientation::OrientationState::Delivered;
    assert!(!session_start(&mut app, pane, "structured", "startup", 1).contains("context"));
    assert_eq!(
        app.state.terminals[&terminal_id].orientation,
        crate::agent_orientation::OrientationState::Delivered
    );
    let offer = session_start(&mut app, pane, "next", "startup", 2);
    assert!(offer.contains("Managed by Herdr"));
    assert_eq!(
        app.state.terminals[&terminal_id].orientation,
        crate::agent_orientation::OrientationState::Offered
    );
    assert!(acknowledge(&mut app, pane, "next", &offer).contains("result"));
    assert!(!session_start(&mut app, pane, "next", "startup", 3).contains("context"));
    app.state
        .terminals
        .get_mut(&terminal_id)
        .unwrap()
        .orientation = crate::agent_orientation::OrientationState::Pending;
    assert!(session_start(&mut app, pane, "resumed", "resume", 4).contains("context"));
    assert_eq!(
        app.state.terminals[&terminal_id].orientation,
        crate::agent_orientation::OrientationState::Offered
    );
}

#[tokio::test]
async fn orientation_offered_blocks_task_until_hook_emission_is_acknowledged() {
    let (mut app, pane) = app_with_two_panes();
    let terminal_id = app.state.terminal_id_for_any_pane(pane).unwrap();
    app.state
        .terminals
        .get_mut(&terminal_id)
        .unwrap()
        .detected_agent = Some(crate::detect::Agent::Codex);
    let offer = session_start(&mut app, pane, "external", "startup", 1);
    let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
    app.terminal_runtimes.insert(terminal_id.clone(), runtime);
    let task = "  exact task\n";
    assert_eq!(
        app.send_agent_task(pane, task).unwrap_err().0,
        "orientation_delivery_pending"
    );
    assert!(rx.try_recv().is_err());
    assert_eq!(
        app.state.terminals[&terminal_id].orientation,
        crate::agent_orientation::OrientationState::Offered
    );
    assert!(acknowledge(&mut app, pane, "external", &offer).contains("result"));
    app.send_agent_task(pane, task).unwrap();
    assert_eq!(
        submitted_bytes(&mut rx).await,
        format!("{task}\r").as_bytes()
    );
}

#[tokio::test]
async fn orientation_offer_retries_after_lost_response_and_only_acknowledgement_delivers() {
    let (mut app, pane) = app_with_two_panes();
    let terminal_id = app.state.terminal_id_for_any_pane(pane).unwrap();
    app.state
        .terminals
        .get_mut(&terminal_id)
        .unwrap()
        .detected_agent = Some(crate::detect::Agent::Codex);
    let first = session_start(&mut app, pane, "external", "startup", 1);
    let retry = session_start(&mut app, pane, "external", "startup", 2);
    let first_value: serde_json::Value = serde_json::from_str(&first).unwrap();
    let retry_value: serde_json::Value = serde_json::from_str(&retry).unwrap();
    assert_eq!(
        first_value["result"]["nonce"],
        retry_value["result"]["nonce"]
    );
    assert_eq!(
        first_value["result"]["context"],
        retry_value["result"]["context"]
    );
    assert_eq!(
        app.state.terminals[&terminal_id].orientation,
        crate::agent_orientation::OrientationState::Offered
    );
    // A bad nonce models a lost/timed-out hook response: it must not deliver.
    let bad = app.handle_pane_ack_orientation(
        "bad".into(),
        PaneAckOrientationParams {
            pane_id: format!("p_{}", pane.raw()),
            source: "herdr:codex".into(),
            agent_session_id: "external".into(),
            nonce: "lost".into(),
        },
    );
    assert!(bad.contains("invalid_orientation_ack"));
    assert_eq!(
        app.state.terminals[&terminal_id].orientation,
        crate::agent_orientation::OrientationState::Offered
    );
    assert!(acknowledge(&mut app, pane, "external", &retry).contains("result"));
    assert_eq!(
        app.state.terminals[&terminal_id].orientation,
        crate::agent_orientation::OrientationState::Delivered
    );
    assert!(acknowledge(&mut app, pane, "external", &retry).contains("result"));
}

#[tokio::test]
async fn team_view_updates_are_coalesced_and_bound_to_holder_scope_revision_and_session() {
    let (mut app, pane) = app_with_two_panes();
    let terminal_id = app.state.terminal_id_for_any_pane(pane).unwrap();
    app.state
        .terminals
        .get_mut(&terminal_id)
        .unwrap()
        .detected_agent = Some(crate::detect::Agent::Codex);
    app.state.toggle_space_overseer(pane);
    let startup = session_start(&mut app, pane, "external", "startup", 1);
    assert!(startup.contains("orientation_context"), "{startup}");
    acknowledge(&mut app, pane, "external", &startup);

    let unchanged = session_start(&mut app, pane, "external", "user_prompt_submit", 2);
    assert!(
        !unchanged.contains("orientation_context"),
        "unchanged view was offered twice: {unchanged}"
    );

    let other = app.state.workspaces[0].tabs[0]
        .panes
        .values()
        .find(|state| state.attached_terminal_id != terminal_id)
        .map(|state| state.attached_terminal_id.clone())
        .unwrap();
    app.state.terminals.get_mut(&other).unwrap().state = crate::detect::AgentState::Working;
    let changed = session_start(&mut app, pane, "external", "user_prompt_submit", 3);
    assert!(
        changed.contains("orientation_context"),
        "changed view was not offered: {changed}"
    );
    acknowledge(&mut app, pane, "external", &changed);
    let duplicate = session_start(&mut app, pane, "external", "user_prompt_submit", 4);
    assert!(
        !duplicate.contains("orientation_context"),
        "coalesced revision was reoffered: {duplicate}"
    );

    let stale = serde_json::from_str::<serde_json::Value>(&changed).unwrap();
    let old_nonce = stale["result"]["nonce"].as_str().unwrap().to_string();
    app.state.toggle_space_overseer(pane);
    let rejected = app.handle_pane_ack_orientation(
        "stale".into(),
        PaneAckOrientationParams {
            pane_id: format!("p_{}", pane.raw()),
            source: "herdr:codex".into(),
            agent_session_id: "external".into(),
            nonce: old_nonce,
        },
    );
    assert!(
        rejected.contains("invalid_orientation_ack"),
        "stale role offer acknowledged: {rejected}"
    );
}

#[tokio::test]
async fn claude_codex_native_clear_and_compact_events_reissue_the_role_card() {
    for (index, agent) in ["claude", "codex"].into_iter().enumerate() {
        let (mut app, pane) = app_with_two_panes();
        let terminal_id = app.state.terminal_id_for_any_pane(pane).unwrap();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .detected_agent = Some(if agent == "claude" {
            crate::detect::Agent::Claude
        } else {
            crate::detect::Agent::Codex
        });
        app.state.toggle_space_overseer(pane);

        let startup =
            session_start_as(&mut app, pane, "native-conversation", "startup", 100, agent);
        assert!(
            startup.contains("orientation_context"),
            "{agent}: {startup}"
        );
        acknowledge_as(&mut app, pane, "native-conversation", &startup, agent);

        let compact =
            session_start_as(&mut app, pane, "native-conversation", "compact", 101, agent);
        assert!(
            compact.contains("orientation_context"),
            "{agent} compact must re-inject the role card: {compact}"
        );
        acknowledge_as(&mut app, pane, "native-conversation", &compact, agent);

        let clear = session_start_as(&mut app, pane, "native-conversation", "clear", 102, agent);
        assert!(
            clear.contains("orientation_context"),
            "{agent} clear must re-inject the role card: {clear}"
        );
        acknowledge_as(&mut app, pane, "native-conversation", &clear, agent);

        let restarted = session_start_as(
            &mut app,
            pane,
            &format!("new-conversation-{index}"),
            "startup",
            103,
            agent,
        );
        assert!(
            restarted.contains("orientation_context"),
            "{agent} restart receives its own card: {restarted}"
        );
    }
}

#[test]
fn claude_native_hook_receives_the_changed_team_view_as_deferred_context() {
    let (mut app, pane) = app_with_two_panes();
    let terminal_id = app.state.terminal_id_for_any_pane(pane).unwrap();
    app.state
        .terminals
        .get_mut(&terminal_id)
        .unwrap()
        .detected_agent = Some(crate::detect::Agent::Claude);
    app.state.toggle_space_overseer(pane);
    let report = |app: &mut App, source: &str, seq| {
        app.handle_pane_report_agent_session(
            "claude-hook".into(),
            PaneReportAgentSessionParams {
                turn_reporting: false,
                startup_source: Some(source.into()),
                pane_id: format!("p_{}", pane.raw()),
                source: "herdr:claude".into(),
                agent: "claude".into(),
                seq: Some(seq),
                agent_session_id: Some("native".into()),
                agent_session_path: None,
            },
        )
    };
    let startup = report(&mut app, "startup", 1);
    assert!(startup.contains("orientation_context"), "{startup}");
    let startup_nonce = serde_json::from_str::<serde_json::Value>(&startup).unwrap()["result"]
        ["nonce"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(app
        .handle_pane_ack_orientation(
            "ack-start".into(),
            PaneAckOrientationParams {
                pane_id: format!("p_{}", pane.raw()),
                source: "herdr:claude".into(),
                agent_session_id: "native".into(),
                nonce: startup_nonce,
            }
        )
        .contains("result"));
    let other = app.state.workspaces[0].tabs[0]
        .panes
        .values()
        .find(|state| state.attached_terminal_id != terminal_id)
        .map(|state| state.attached_terminal_id.clone())
        .unwrap();
    app.state.terminals.get_mut(&other).unwrap().state = crate::detect::AgentState::Working;
    let update = report(&mut app, "user_prompt_submit", 2);
    assert!(update.contains("orientation_context"), "{update}");
}

#[tokio::test]
async fn orientation_navigation_and_empty_input_preserve_pending_and_raw_batches_are_atomic() {
    let (mut app, pane) = app_with_two_panes();
    let terminal_id = app.state.terminal_id_for_any_pane(pane).unwrap();
    app.state
        .terminals
        .get_mut(&terminal_id)
        .unwrap()
        .orientation = crate::agent_orientation::OrientationState::Pending;
    let address = app.public_pane_id(0, pane).unwrap();
    let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 2);
    app.terminal_runtimes.insert(terminal_id.clone(), runtime);
    for keys in [vec!["Up", "Esc"], vec!["Enter"]] {
        assert!(app
            .handle_pane_send_keys(
                "navigation".into(),
                PaneSendKeysParams {
                    pane_id: address.clone(),
                    keys: keys.into_iter().map(String::from).collect(),
                }
            )
            .contains("result"));
        rx.try_recv().unwrap();
        assert_eq!(
            app.state.terminals[&terminal_id].orientation,
            crate::agent_orientation::OrientationState::Pending
        );
    }
    assert!(app
        .handle_agent_send(
            "empty".into(),
            crate::api::schema::AgentSendParams {
                target: terminal_id.to_string(),
                text: String::new()
            }
        )
        .contains("result"));
    rx.try_recv().unwrap();
    assert_eq!(
        app.state.terminals[&terminal_id].orientation,
        crate::agent_orientation::OrientationState::Pending
    );
    for code in [
        crossterm::event::KeyCode::Up,
        crossterm::event::KeyCode::Esc,
        crossterm::event::KeyCode::Enter,
    ] {
        app.handle_terminal_key_headless(
            crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::empty()).into(),
        );
        rx.try_recv().unwrap();
        assert_eq!(
            app.state.terminals[&terminal_id].orientation,
            crate::agent_orientation::OrientationState::Pending
        );
    }
    // With one slot occupied, the old text/Enter/Down loop admitted a partial task.
    app.terminal_runtimes
        .get(&terminal_id)
        .unwrap()
        .try_send_bytes(Bytes::from_static(b"occupied"))
        .unwrap();
    assert!(app
        .handle_pane_send_input(
            "batch".into(),
            PaneSendInputParams {
                pane_id: address.clone(),
                text: "literal task".into(),
                keys: vec!["Enter".into(), "Down".into()],
                origin: None,
                correlation_id: None,
                task_id: None,
            }
        )
        .contains("result"));
    assert_eq!(&rx.try_recv().unwrap()[..], b"occupied");
    let batch = rx.try_recv().unwrap();
    assert!(batch.starts_with(b"literal task"));
    if batch.contains(&b'\r') {
        assert!(rx.try_recv().is_err());
    } else {
        assert!(rx.try_recv().is_err(), "Enter waits out the settle delay");
        let keys = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("delayed keys")
            .expect("keys");
        assert!(keys.starts_with(b"\r"));
    }
    assert_eq!(
        app.state.terminals[&terminal_id].orientation,
        crate::agent_orientation::OrientationState::Suppressed
    );
    app.state
        .terminals
        .get_mut(&terminal_id)
        .unwrap()
        .orientation = crate::agent_orientation::OrientationState::Pending;
    for _ in 0..2 {
        app.terminal_runtimes
            .get(&terminal_id)
            .unwrap()
            .try_send_bytes(Bytes::from_static(b"full"))
            .unwrap();
    }
    assert!(app
        .handle_pane_send_input(
            "full".into(),
            PaneSendInputParams {
                pane_id: address,
                text: "not admitted".into(),
                keys: vec!["Enter".into(), "Down".into()],
                origin: None,
                correlation_id: None,
                task_id: None,
            }
        )
        .contains("pane_send_failed"));
    assert_eq!(
        app.state.terminals[&terminal_id].orientation,
        crate::agent_orientation::OrientationState::Pending
    );
    for _ in 0..2 {
        rx.try_recv().unwrap();
    }
    app.send_agent_task(pane, "first real task").unwrap();
    assert!(rx
        .try_recv()
        .unwrap()
        .starts_with(crate::agent_orientation::OPEN.as_bytes()));
}

#[test]
fn lifecycle_refreshes_after_an_acknowledged_startup_get_their_own_nonces() {
    let (mut app, pane) = app_with_two_panes();
    let terminal_id = app.state.terminal_id_for_any_pane(pane).unwrap();
    app.state
        .terminals
        .get_mut(&terminal_id)
        .unwrap()
        .detected_agent = Some(crate::detect::Agent::Claude);
    app.state.toggle_space_overseer(pane);
    let nonce_of = |response: &str| {
        serde_json::from_str::<serde_json::Value>(response).unwrap()["result"]["nonce"]
            .as_str()
            .unwrap()
            .to_string()
    };

    let startup = session_start_as(
        &mut app,
        pane,
        "native-conversation",
        "startup",
        100,
        "claude",
    );
    let startup_nonce = nonce_of(&startup);
    acknowledge_as(&mut app, pane, "native-conversation", &startup, "claude");

    // Same conversation, role revision and team state: a compact is still a new delivery.
    let compact = session_start_as(
        &mut app,
        pane,
        "native-conversation",
        "compact",
        101,
        "claude",
    );
    assert!(compact.contains("orientation_context"), "{compact}");
    let compact_nonce = nonce_of(&compact);
    assert_ne!(compact_nonce, startup_nonce);
    // A retry of the still pending compact offer keeps its nonce.
    let retry = session_start_as(
        &mut app,
        pane,
        "native-conversation",
        "compact",
        102,
        "claude",
    );
    assert_eq!(nonce_of(&retry), compact_nonce);
    acknowledge_as(&mut app, pane, "native-conversation", &compact, "claude");

    let resume = session_start_as(
        &mut app,
        pane,
        "native-conversation",
        "resume",
        103,
        "claude",
    );
    assert!(resume.contains("orientation_context"), "{resume}");
    let resume_nonce = nonce_of(&resume);
    assert_ne!(resume_nonce, compact_nonce);
    assert_ne!(resume_nonce, startup_nonce);
    // Terminal, session, role revision, team revision, generation: the hook test relies on this shape.
    assert_eq!(resume_nonce.split(':').count(), 5, "{resume_nonce}");
    assert!(resume_nonce.ends_with(":3"), "{resume_nonce}");
}

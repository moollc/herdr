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
    app.handle_pane_report_agent_session(
        "startup".into(),
        PaneReportAgentSessionParams {
            pane_id: format!("p_{}", pane.raw()),
            source: "herdr:codex".into(),
            agent: "codex".into(),
            startup_source: Some(source.into()),
            seq: Some(seq),
            agent_session_id: Some(session.into()),
            agent_session_path: None,
        },
    )
}

fn acknowledge(app: &mut App, pane: crate::layout::PaneId, session: &str, offer: &str) -> String {
    let response: serde_json::Value = serde_json::from_str(offer).unwrap();
    let nonce = response["result"]["nonce"].as_str().unwrap();
    app.handle_pane_ack_orientation(
        "ack".into(),
        PaneAckOrientationParams {
            pane_id: format!("p_{}", pane.raw()),
            source: "herdr:codex".into(),
            agent_session_id: session.into(),
            nonce: nonce.into(),
        },
    )
}

#[tokio::test]
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
        },
    );
    assert!(response.contains("result"));
    assert_eq!(&rx.try_recv().unwrap()[..], format!("{task}\r").as_bytes());
    assert!(
        !session_start(&mut app, pane, "external-codex", "startup", u64::MAX - 2)
            .contains("context")
    );
    assert!(
        !session_start(&mut app, pane, "external-codex", "resume", u64::MAX - 1)
            .contains("context")
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
    assert!(!session_start(&mut app, pane, "resumed", "resume", 4).contains("context"));
    assert_eq!(
        app.state.terminals[&terminal_id].orientation,
        crate::agent_orientation::OrientationState::Suppressed
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
            }
        )
        .contains("result"));
    assert_eq!(&rx.try_recv().unwrap()[..], b"occupied");
    let batch = rx.try_recv().unwrap();
    assert!(batch.starts_with(b"literal task\r"));
    assert!(rx.try_recv().is_err());
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

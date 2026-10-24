//! Managed native turn hook installation and reversible notification wrapping.
use super::*;
const CURSOR_HOOK: &str = include_str!("assets/cursor/herdr-agent-state.sh");
const TURN_HELPER: &str = include_str!("assets/turn-report.py");
const RECENT_AGENT_FILES_HOOK_NAME: &str = "herdr-recent-agent-files.py";
const RECENT_AGENT_FILES_HOOK_ASSET: &str = include_str!("assets/recent-agent-files.py");
const BEGIN: &str = "# >>> herdr turn notifications";
const END: &str = "# <<< herdr turn notifications";

pub(super) fn write_helper(dir: &Path) -> io::Result<()> {
    fs::write(dir.join("herdr-turn-report.py"), TURN_HELPER)?;
    fs::write(
        dir.join(RECENT_AGENT_FILES_HOOK_NAME),
        RECENT_AGENT_FILES_HOOK_ASSET,
    )
}

pub(super) fn cursor_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home("CURSOR_CONFIG_DIR", &[".cursor"])
}
pub(super) fn install_cursor() -> io::Result<Vec<String>> {
    let dir = cursor_dir()?;
    fs::create_dir_all(&dir)?;
    let path = dir.join("herdr-agent-state.sh");
    fs::write(&path, CURSOR_HOOK)?;
    make_executable(&path)?;
    write_helper(&dir)?;
    cursor_hooks(&dir, false)?;
    Ok(vec![format!(
        "installed cursor turn hooks at {}",
        path.display()
    )])
}
pub(super) fn cursor_registered(script: &Path) -> bool {
    let Some(dir) = script.parent() else {
        return false;
    };
    let settings = fs::read_to_string(dir.join("hooks.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    let quoted = shell_single_quote(&script.display().to_string());
    [
        ("sessionStart", "session"),
        ("beforeSubmitPrompt", "started"),
        ("stop", "ended"),
    ]
    .iter()
    .all(|(event, action)| {
        let command = format!("sh {quoted} {action}");
        settings
            .as_ref()
            .and_then(|s| s.get("hooks"))
            .and_then(|h| h.get(event))
            .and_then(Value::as_array)
            .is_some_and(|entries| {
                entries
                    .iter()
                    .any(|e| e.get("command").and_then(Value::as_str) == Some(command.as_str()))
            })
    }) && settings
        .as_ref()
        .and_then(|s| s.get("hooks"))
        .and_then(|h| h.get("afterFileEdit"))
        .and_then(Value::as_array)
        .is_some_and(|entries| {
            let command = format!(
                "python3 {} cursor afterFileEdit",
                shell_single_quote(&dir.join(RECENT_AGENT_FILES_HOOK_NAME).display().to_string())
            );
            entries
                .iter()
                .any(|entry| entry.get("command").and_then(Value::as_str) == Some(command.as_str()))
        })
        && settings
            .as_ref()
            .and_then(|s| s.get("hooks"))
            .and_then(|h| h.get("postToolUse"))
            .and_then(Value::as_array)
            .is_some_and(|entries| {
                let command = format!(
                    "python3 {} cursor postToolUse",
                    shell_single_quote(
                        &dir.join(RECENT_AGENT_FILES_HOOK_NAME).display().to_string()
                    )
                );
                entries.iter().any(|entry| {
                    entry.get("command").and_then(Value::as_str) == Some(command.as_str())
                        && entry.get("matcher").and_then(Value::as_str) == Some("Shell")
                })
            })
}
pub(super) fn uninstall_cursor() -> io::Result<Vec<String>> {
    let dir = cursor_dir()?;
    cursor_hooks(&dir, true)?;
    // Managed assets are inert without registrations; retain them for rollback.
    Ok(vec!["removed cursor turn hook registrations".into()])
}
fn cursor_hooks(dir: &Path, remove: bool) -> io::Result<()> {
    let path = dir.join("hooks.json");
    let mut settings: Value = if path.exists() {
        serde_json::from_str(&fs::read_to_string(&path)?)?
    } else {
        json!({"version": 1})
    };
    let hooks = ensure_hooks_object(&mut settings, &path, "cursor hooks", "cursor hooks")?;
    let script = shell_single_quote(&dir.join("herdr-agent-state.sh").display().to_string());
    for (event, action) in [
        ("sessionStart", "session"),
        ("beforeSubmitPrompt", "started"),
        ("stop", "ended"),
    ] {
        let command = format!("sh {script} {action}");
        let entries = hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| io::Error::other("cursor hook must be an array"))?;
        entries.retain(|e| e.get("command").and_then(Value::as_str) != Some(command.as_str()));
        if !remove {
            entries.push(json!({"command": command}));
        }
        if entries.is_empty() {
            hooks.remove(event);
        }
    }
    let recent_script =
        shell_single_quote(&dir.join(RECENT_AGENT_FILES_HOOK_NAME).display().to_string());
    let event = "afterFileEdit";
    let command = format!("python3 {recent_script} cursor {event}");
    let entries = hooks
        .entry(event.to_string())
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| io::Error::other("cursor afterFileEdit hooks must be an array"))?;
    entries.retain(|entry| entry.get("command").and_then(Value::as_str) != Some(command.as_str()));
    if !remove {
        entries.push(json!({"command": command}));
    }
    if entries.is_empty() {
        hooks.remove(event);
    }
    let event = "postToolUse";
    let command = format!("python3 {recent_script} cursor {event}");
    let entries = hooks
        .entry(event.to_string())
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| io::Error::other("cursor postToolUse hooks must be an array"))?;
    entries.retain(|entry| entry.get("command").and_then(Value::as_str) != Some(command.as_str()));
    if !remove {
        entries.push(json!({"command": command, "matcher": "Shell"}));
    }
    if entries.is_empty() {
        hooks.remove(event);
    }
    fs::write(path, serde_json::to_string_pretty(&settings)?)
}

/// Locate the original root notify statement, including multiline arrays. Parse
/// TOML before any mutation; preserve every byte outside that statement.
fn notify_span(content: &str) -> io::Result<Option<std::ops::Range<usize>>> {
    let mut offset = 0;
    for line in content.split_inclusive('\n') {
        if line.trim_start().starts_with('[') {
            break;
        }
        let key = line
            .split_once('=')
            .map(|(k, _)| k.trim().trim_matches(['\'', '"']));
        if key == Some("notify") {
            for end in content[offset..]
                .match_indices('\n')
                .map(|(i, _)| offset + i + 1)
                .chain(std::iter::once(content.len()))
            {
                if toml::from_str::<toml::Value>(&content[offset..end]).is_ok() {
                    return Ok(Some(offset..end));
                }
            }
            return Err(io::Error::other("cannot locate complete notify statement"));
        }
        offset += line.len();
    }
    Ok(None)
}

pub(super) fn configure_notify(content: &str, dir: &Path, remove: bool) -> io::Result<String> {
    let parsed: toml::Value = toml::from_str(content).map_err(io::Error::other)?;
    if let Some(start) = content.find(BEGIN) {
        let end = content[start..]
            .find(END)
            .ok_or_else(|| io::Error::other("incomplete managed notify block"))?
            + start
            + END.len();
        let end = end + usize::from(content[end..].starts_with('\n'));
        if !remove {
            return Ok(content.to_string());
        }
        let original: String =
            serde_json::from_str(&fs::read_to_string(dir.join("herdr-notify-original.json"))?)?;
        return Ok(format!(
            "{}{}{}",
            &content[..start],
            original,
            &content[end..]
        ));
    }
    if remove {
        return Ok(content.to_string());
    }
    let delegate: Vec<String> = match parsed.get("notify") {
        Some(value) => value
            .as_array()
            .ok_or_else(|| io::Error::other("notify must be a command array"))?
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| io::Error::other("notify arguments must be strings"))
            })
            .collect::<io::Result<_>>()?,
        None => Vec::new(),
    };
    let span = notify_span(content)?.unwrap_or(0..0);
    if parsed.get("notify").is_some() && span.is_empty() {
        return Err(io::Error::other(
            "unsupported notify syntax; original configuration retained",
        ));
    }
    fs::write(
        dir.join("herdr-notify-original.json"),
        serde_json::to_string(&content[span.clone()])?,
    )?;
    fs::write(
        dir.join("herdr-notify-delegate.json"),
        serde_json::to_string(&delegate)?,
    )?;
    let command = vec![
        "sh".to_string(),
        dir.join(CODEX_HOOK_INSTALL_NAME).display().to_string(),
        "notify".to_string(),
    ];
    let block = format!(
        "{BEGIN}\nnotify = {}\n{END}\n",
        serde_json::to_string(&command)?
    );
    Ok(format!(
        "{}{}{}",
        &content[..span.start],
        block,
        &content[span.end..]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_turn_notify_wrapper_is_reversible_and_idempotent() {
        let dir = std::env::temp_dir().join(format!(
            "turn-notify-{}",
            crate::terminal::TerminalId::alloc()
        ));
        fs::create_dir_all(&dir).unwrap();
        for original in [
            "",
            r#"model = "test""#,
            "# custom\nnotify = [\n  'echo', 'keep' # comment\n]\n[features]\nhooks = true\n",
            "'notify' = ['echo', 'keep']\nmodel='test'\n",
        ] {
            let installed = configure_notify(original, &dir, false).unwrap();
            let parsed: toml::Value = toml::from_str(&installed).unwrap();
            assert_eq!(parsed["notify"][2].as_str(), Some("notify"));
            assert_eq!(
                configure_notify(&installed, &dir, false).unwrap(),
                installed
            );
            assert_eq!(configure_notify(&installed, &dir, true).unwrap(), original);
        }
    }
    #[test]
    fn native_turn_cursor_hooks_preserve_user_hooks() {
        let dir = std::env::temp_dir().join(format!(
            "turn-cursor-{}",
            crate::terminal::TerminalId::alloc()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("hooks.json"),
            r#"{"version":1,"hooks":{"stop":[{"command":"echo keep","loop_limit":3}],"postToolUse":[{"command":"echo audit","matcher":"Shell"}]}}"#,
        )
        .unwrap();
        cursor_hooks(&dir, false).unwrap();
        cursor_hooks(&dir, false).unwrap();
        let parsed: Value =
            serde_json::from_str(&fs::read_to_string(dir.join("hooks.json")).unwrap()).unwrap();
        assert_eq!(parsed["hooks"]["stop"].as_array().unwrap().len(), 2);
        assert_eq!(
            parsed["hooks"]["afterFileEdit"].as_array().unwrap().len(),
            1
        );
        assert!(parsed["hooks"]["afterFileEdit"][0]["command"]
            .as_str()
            .unwrap()
            .contains("cursor afterFileEdit"));
        assert!(parsed["hooks"]["postToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| {
                entry["command"]
                    .as_str()
                    .unwrap()
                    .contains("cursor postToolUse")
                    && entry["matcher"] == "Shell"
            }));
        assert!(parsed["hooks"]["postToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| { entry["command"] == "echo audit" && entry["matcher"] == "Shell" }));
        cursor_hooks(&dir, true).unwrap();
        let restored: Value =
            serde_json::from_str(&fs::read_to_string(dir.join("hooks.json")).unwrap()).unwrap();
        assert_eq!(
            restored["hooks"]["stop"],
            json!([{"command":"echo keep","loop_limit":3}])
        );
        assert!(restored["hooks"].get("sessionStart").is_none());
        assert!(restored["hooks"].get("afterFileEdit").is_none());
        assert_eq!(
            restored["hooks"]["postToolUse"],
            json!([{"command":"echo audit","matcher":"Shell"}])
        );
    }
}

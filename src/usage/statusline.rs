//! Claude's fast path. No provider scan, shell startup, or network request.
use super::{atomic_json, claude, countdown::countdown, palette, read_json, UsageConfig};
use serde_json::{json, Value};
use std::{
    io::{self, Read},
    path::Path,
};

/// How the numbers in one status line call relate to this session's last call.
/// Claude Code sends the rate limits of the session's last API response, and
/// redraws them unchanged while idle. `cost.total_api_duration_ms` only grows when
/// a new response arrives, so it tells a new response from a redraw.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reading {
    /// A new response since this session was last seen: authoritative.
    Fresh,
    /// The same response redrawn: older than what is saved.
    Stale,
    /// A session or marker not seen before: cannot tell, so it can only raise.
    Unknown,
}
const MAX_SESSIONS: usize = 32;

/// Same reset time means the same window. An earlier reset time is an old window and
/// is ignored, a later one replaces the window. In the same window a fresh reading
/// replaces the saved one even when lower (a manual reset), a stale one changes
/// nothing, and an unknown one never lowers it.
fn merge_window(old: &Value, new: &Value, reading: Reading) -> Value {
    let reset = |v: &Value| v.get("resets_at").and_then(Value::as_f64);
    let used = |v: &Value| v.get("used_percentage").and_then(Value::as_f64);
    match (reset(old), reset(new)) {
        (Some(o), Some(n)) if n < o => old.clone(),
        (Some(o), Some(n)) if n == o => match (reading, used(old), used(new)) {
            (Reading::Fresh, _, _) => new.clone(),
            (Reading::Stale, _, _) => old.clone(),
            (Reading::Unknown, Some(a), Some(b)) if a > b => old.clone(),
            _ => new.clone(),
        },
        _ => new.clone(),
    }
}
pub(super) fn save_newer(path: &Path, state: &Value) -> io::Result<()> {
    let _lock = crate::platform::usage::lock_statusline(path)?;
    let seen = state
        .get("seen_at")
        .and_then(Value::as_u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing seen_at"))?;
    let saved = read_json::<Value>(path).ok();
    let old = saved
        .as_ref()
        .and_then(|v| v.get("seen_at").and_then(Value::as_u64));
    // Remember each session's last marker, even when its numbers are not saved.
    let mut sessions = saved
        .as_ref()
        .and_then(|v| v.get("sessions"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut reading = Reading::Unknown;
    if let (Some(id), Some(marker)) = (
        state.pointer("/session/id").and_then(Value::as_str),
        state.pointer("/session/marker").and_then(Value::as_u64),
    ) {
        let before = sessions
            .get(id)
            .and_then(|v| v.get("marker"))
            .and_then(Value::as_u64);
        reading = match before {
            Some(b) if marker > b => Reading::Fresh,
            Some(b) if marker == b => Reading::Stale,
            _ => Reading::Unknown,
        };
        sessions.insert(id.to_string(), json!({"marker": marker, "seen_at": seen}));
        while sessions.len() > MAX_SESSIONS {
            let oldest = sessions
                .iter()
                .min_by_key(|(_, v)| v.get("seen_at").and_then(Value::as_u64).unwrap_or(0))
                .map(|(k, _)| k.clone());
            match oldest {
                Some(key) => sessions.remove(&key),
                None => break,
            };
        }
    }
    let mut next = match (&saved, old.is_none_or(|old| seen > old)) {
        (Some(saved), false) => saved.clone(),
        _ => {
            let mut next = state.clone();
            if let Some(saved) = &saved {
                for key in ["five_hour", "seven_day"] {
                    if let (Some(o), Some(n)) = (saved.get(key), state.get(key)) {
                        next[key] = merge_window(o, n, reading);
                    }
                }
            }
            next
        }
    };
    next.as_object_mut().map(|object| object.remove("session"));
    if !sessions.is_empty() {
        next["sessions"] = Value::Object(sessions);
    }
    if saved.as_ref() != Some(&next) {
        atomic_json(path, &next)?;
    }
    Ok(())
}
pub fn run(
    config: &UsageConfig,
    input: impl Read,
    output: &mut impl io::Write,
    now: u64,
) -> io::Result<()> {
    let mut bytes = Vec::new();
    input.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let mut state = json!({"seen_at": now});
    if let Some(limits) = value.get("rate_limits") {
        if let Some(object) = limits.as_object() {
            for key in ["five_hour", "seven_day"] {
                if let Some(v) = object.get(key) {
                    state[key] = v.clone();
                }
            }
            // The session and its API time tell a new response from a redraw.
            if let (Some(id), Some(marker)) = (
                value.get("session_id").and_then(Value::as_str),
                value
                    .pointer("/cost/total_api_duration_ms")
                    .and_then(Value::as_u64),
            ) {
                state["session"] = json!({"id": id, "marker": marker});
            }
            save_newer(&config.claude_state_file, &state)?;
        }
    }
    if !config.claude_statusline || config.visibility_file.as_deref().is_some_and(usage_hidden) {
        return writeln!(output);
    }
    let reading = claude::parse(&state, now);
    for (i, w) in reading.windows.iter().enumerate() {
        if i > 0 {
            write!(output, "  ")?;
        }
        let (r, g, b) = palette::level_rgb(reading.provider, w.kind, w.left_pct);
        let (er, eg, eb) = palette::EMPTY;
        let lit = w.left_pct.map_or(0, |v| usize::from(v).div_ceil(10));
        write!(
            output,
            "\x1b[38;2;{r};{g};{b}m{}\x1b[38;2;{er};{eg};{eb}m{}\x1b[0m {} {}",
            "■".repeat(lit),
            "■".repeat(10 - lit),
            w.left_pct.map_or("?".into(), |n| format!("{n}%")),
            countdown(w.resets_at, now).text
        )?;
    }
    writeln!(output)
}

/// A tiny per-session projection of the server's visibility state.
/// Reading it never opens or parses the session snapshot.
pub(crate) fn visibility_file() -> std::path::PathBuf {
    visibility_file_for(
        &crate::config::state_dir().join("usage"),
        crate::session::active_name().as_deref(),
    )
}
fn visibility_file_for(state: &Path, session: Option<&str>) -> std::path::PathBuf {
    state
        .join("visibility")
        .join(format!("{}.json", session.unwrap_or("default")))
}
pub(crate) fn save_visibility(path: &Path, hidden: bool) -> io::Result<()> {
    atomic_json(path, &hidden)
}
fn usage_hidden(path: &Path) -> bool {
    std::fs::File::open(path)
        .ok()
        .and_then(|file| serde_json::from_reader::<_, bool>(file.take(16)).ok())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn usage_statusline_off_still_records_and_on_prints_meters() {
        for (enabled, hidden) in [(false, false), (true, true), (true, false)] {
            let mut config = crate::usage::tests::config();
            config.claude_statusline = enabled;
            let session = config.snapshot_file.with_extension("session");
            std::fs::write(&session, hidden.to_string()).unwrap();
            config.visibility_file = Some(session);
            let mut output = Vec::new();
            run(
                &config,
                br#"{"rate_limits":{"five_hour":{"used_percentage":20,"resets_at":2000000000}}}"#
                    .as_slice(),
                &mut output,
                100,
            )
            .unwrap();
            let saved: Value = read_json(&config.claude_state_file).unwrap();
            assert_eq!(saved["five_hour"]["used_percentage"], 20);
            if enabled && !hidden {
                assert!(String::from_utf8(output).unwrap().contains("80%"));
            } else {
                assert_eq!(output, b"\n");
            }
        }
    }
    #[test]
    fn usage_statusline_visibility_tracks_the_saved_session_and_defaults_on() {
        let mut config = crate::usage::tests::config();
        let path = config.snapshot_file.with_extension("session");
        config.visibility_file = Some(path.clone());
        for text in ["{}", "{", "false"] {
            std::fs::write(&path, text).unwrap();
            assert!(!usage_hidden(&path));
        }
        std::fs::write(&path, "true").unwrap();
        let mut out = Vec::new();
        run(&config, b"{}".as_slice(), &mut out, 100).unwrap();
        assert_eq!(out, b"\n");
        std::fs::write(&path, "false").unwrap();
        out.clear();
        run(&config, b"{}".as_slice(), &mut out, 101).unwrap();
        assert!(out.len() > 1);
    }
    #[test]
    fn usage_visibility_marker_ignores_huge_session_and_is_session_scoped() {
        let mut config = crate::usage::tests::config();
        let root = config.snapshot_file.parent().unwrap();
        let marker = visibility_file_for(root, None);
        let named = visibility_file_for(root, Some("synthetic"));
        assert_ne!(marker, named);
        assert!(!usage_hidden(&marker), "missing marker means shown");
        let snapshot = root.join("session.json");
        std::fs::write(
            &snapshot,
            format!(
                r#"{{"usage_hidden":true,"ignored":"{}"}}"#,
                "x".repeat(20 * 1024 * 1024)
            ),
        )
        .unwrap();
        assert!(std::fs::metadata(&snapshot).unwrap().len() > 16 * 1024 * 1024);
        config.visibility_file = Some(marker.clone());
        save_visibility(&marker, true).unwrap();
        let mut output = Vec::new();
        run(
            &config,
            br#"{"rate_limits":{"five_hour":{"used_percentage":20,"resets_at":2000000000}}}"#
                .as_slice(),
            &mut output,
            100,
        )
        .unwrap();
        assert_eq!(output, b"\n");
        assert_eq!(
            read_json::<Value>(&config.claude_state_file).unwrap()["five_hour"]["used_percentage"],
            20
        );
        assert!(!usage_hidden(&named), "one session cannot hide another");
        save_visibility(&marker, false).unwrap();
        output.clear();
        run(&config, b"{}".as_slice(), &mut output, 101).unwrap();
        assert!(
            output.len() > 1,
            "the huge hidden snapshot is never consulted"
        );
    }
}

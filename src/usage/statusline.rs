//! Claude's fast path. No provider scan, shell startup, or network request.
use super::{atomic_json, claude, countdown::countdown, palette, read_json, UsageConfig};
use serde_json::{json, Value};
use std::{
    io::{self, Read},
    path::Path,
};

pub(super) fn save_newer(path: &Path, state: &Value) -> io::Result<()> {
    let _lock = crate::platform::usage::lock_statusline(path)?;
    let seen = state
        .get("seen_at")
        .and_then(Value::as_u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing seen_at"))?;
    let old = read_json::<Value>(path)
        .ok()
        .and_then(|v| v.get("seen_at").and_then(Value::as_u64));
    if old.is_none_or(|old| seen > old) {
        atomic_json(path, state)?;
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

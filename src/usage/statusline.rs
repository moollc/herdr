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

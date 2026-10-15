use super::{
    local_time, timestamp, unknown, window, Provider, ProviderUsage, UsageProblem, WindowKind,
};
use serde::Deserialize;
use serde_json::Value;
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

fn files(folder: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                return None;
            }
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            Some((meta.modified().ok()?, path))
        })
        .collect();
    paths.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    paths.into_iter().take(5).map(|(_, p)| p).collect()
}
fn newest_folder(root: &Path, exclude: &Path) -> Option<PathBuf> {
    // Walk calendar directory names only; never scan session file contents here.
    let mut years: Vec<_> = fs::read_dir(root)
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().len() == 4)
        .collect();
    years.sort_by_key(|e| std::cmp::Reverse(e.file_name()));
    for year in years {
        let mut months: Vec<_> = fs::read_dir(year.path())
            .into_iter()
            .flatten()
            .flatten()
            .collect();
        months.sort_by_key(|e| std::cmp::Reverse(e.file_name()));
        for month in months {
            let mut days: Vec<_> = fs::read_dir(month.path())
                .into_iter()
                .flatten()
                .flatten()
                .collect();
            days.sort_by_key(|e| std::cmp::Reverse(e.file_name()));
            if let Some(day) = days
                .into_iter()
                .find(|e| e.path() < exclude && e.file_type().is_ok_and(|t| t.is_dir()))
            {
                return Some(day.path());
            }
        }
    }
    None
}
fn tail(path: &Path, size: u64) -> std::io::Result<Vec<u8>> {
    let mut f = fs::File::open(path)?;
    let len = f.metadata()?.len();
    let offset = len.saturating_sub(size);
    f.seek(SeekFrom::Start(offset))?;
    let mut data = Vec::with_capacity(size as usize);
    f.take(size).read_to_end(&mut data)?;
    if offset > 0 {
        if let Some(end) = data.iter().position(|b| *b == b'\n') {
            data.drain(..=end);
        } else {
            data.clear();
        }
    }
    Ok(data)
}
#[derive(Deserialize)]
struct Event {
    #[serde(default)]
    timestamp: Value,
    #[serde(rename = "type")]
    kind: String,
    payload: Payload,
}
#[derive(Deserialize)]
struct Payload {
    #[serde(rename = "type")]
    kind: String,
    rate_limits: Limits,
}
#[derive(Deserialize)]
struct Limits {
    limit_id: String,
    #[serde(default)]
    primary: Value,
    #[serde(default)]
    secondary: Value,
}
struct Reading {
    key: (u64, u32),
    seen: Option<u64>,
    limits: Limits,
}
fn candidate(data: &[u8]) -> Option<Reading> {
    // str::lines uses the standard library's fast newline search. A typed envelope
    // skips token totals and other fields without allocating a JSON tree for them.
    String::from_utf8_lossy(data)
        .lines()
        .filter_map(|line| {
            if !line.contains("token_count") {
                return None;
            }
            let event: Event = serde_json::from_str(line).ok()?;
            if event.kind != "event_msg"
                || event.payload.kind != "token_count"
                || event.payload.rate_limits.limit_id != "codex"
            {
                return None;
            }
            let text = event.timestamp.as_str().unwrap_or("");
            let seen = timestamp(text);
            let fraction = text
                .get(19..)
                .and_then(|s| s.strip_prefix('.'))
                .unwrap_or("");
            let mut nanos = 0;
            for byte in fraction
                .bytes()
                .take(9)
                .chain(std::iter::repeat(b'0'))
                .take(9)
            {
                nanos = nanos * 10
                    + u32::from(if byte.is_ascii_digit() {
                        byte - b'0'
                    } else {
                        0
                    });
            }
            Some(Reading {
                key: (seen.unwrap_or(0), nanos),
                seen,
                limits: event.payload.rate_limits,
            })
        })
        .max_by_key(|reading| reading.key)
}
fn latest(paths: &[PathBuf]) -> Option<Reading> {
    paths
        .iter()
        .filter_map(|path| {
            tail(path, 64 * 1024)
                .ok()
                .and_then(|data| candidate(&data))
                .or_else(|| {
                    tail(path, 256 * 1024)
                        .ok()
                        .and_then(|data| candidate(&data))
                })
        })
        .max_by_key(|reading| reading.key)
}
pub fn read(home: &Path, now: u64) -> ProviderUsage {
    let root = home.join("sessions");
    let today = root.join(local_time(now, c"%Y/%m/%d"));
    let paths = files(&today);
    // Reserve one of the five file reads for the previous day if today has no
    // qualifying events. When today has data, its fifth newest file participates.
    let current_count = paths.len().min(4);
    let mut best = latest(&paths[..current_count]);
    if best.is_some() {
        if let Some(fifth) = paths.get(4) {
            if let Some(found) = latest(std::slice::from_ref(fifth)) {
                if best.as_ref().is_none_or(|old| found.key >= old.key) {
                    best = Some(found);
                }
            }
        }
    } else if let Some(folder) = newest_folder(&root, &today) {
        let older = files(&folder);
        best = latest(&older[..older.len().min(5 - current_count)]);
    }
    let Some(reading) = best else {
        return unknown(Provider::Codex, UsageProblem::NoData);
    };
    let mut result = unknown(Provider::Codex, UsageProblem::FormatChanged);
    let Some(seen) = reading.seen else {
        return result;
    };
    result.seen_at = Some(seen);
    let windows = [
        (&reading.limits.primary, WindowKind::FiveHour),
        (&reading.limits.secondary, WindowKind::Weekly),
    ]
    .into_iter()
    .map(|(value, kind)| window(Some(value), kind, "used_percent", now))
    .collect::<Result<Vec<_>, ()>>();
    if let Ok(windows) = windows {
        result.windows = windows;
        result.problem = None;
    }
    result
}

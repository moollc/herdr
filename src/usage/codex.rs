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
    plan_type: Option<String>,
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
/// The newest qualifying reading under one Codex home: five newest files of
/// today, then of the most recent earlier day, tails only.
fn reading_in(home: &Path, now: u64) -> Option<Reading> {
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
    best
}

/// Where a candidate home came from. `active` homes belong to a Codex
/// process that is running now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Home {
    pub path: PathBuf,
    pub active: bool,
}

/// Read every candidate home and show one account's newest reading. Readings
/// that name different plans are never mixed: homes of running Codex
/// processes decide, then the first (configured) home. With no plan named,
/// newest wins.
pub fn read_homes(homes: &[Home], now: u64) -> ProviderUsage {
    let mut seen_paths: Vec<&Path> = Vec::new();
    let mut found: Vec<(bool, Reading)> = Vec::new();
    let mut configured_plan = None;
    for (index, home) in homes.iter().enumerate() {
        if seen_paths.contains(&home.path.as_path()) {
            continue;
        }
        seen_paths.push(&home.path);
        if let Some(reading) = reading_in(&home.path, now) {
            // The first candidate is the configured home and keeps its say.
            if index == 0 {
                configured_plan = reading.limits.plan_type.clone();
            }
            found.push((home.active, reading));
        }
    }
    let plans: std::collections::BTreeSet<_> = found
        .iter()
        .filter_map(|(_, r)| r.limits.plan_type.clone())
        .collect();
    let best = if plans.len() > 1 {
        let any_active = found.iter().any(|(active, _)| *active);
        let chosen_plan = if any_active {
            found
                .iter()
                .filter(|(active, _)| *active)
                .max_by_key(|(_, r)| r.key)
                .and_then(|(_, r)| r.limits.plan_type.clone())
        } else {
            configured_plan
        };
        found
            .into_iter()
            .filter(|(_, r)| chosen_plan.is_none() || r.limits.plan_type == chosen_plan)
            .map(|(_, r)| r)
            .max_by_key(|r| r.key)
    } else {
        found.into_iter().map(|(_, r)| r).max_by_key(|r| r.key)
    };
    build(best, now)
}

pub fn read(home: &Path, now: u64) -> ProviderUsage {
    build(reading_in(home, now), now)
}

fn build(best: Option<Reading>, now: u64) -> ProviderUsage {
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

#[cfg(test)]
mod home_tests {
    use super::*;
    use std::fs;

    const NOW: u64 = 1_792_000_000;

    fn write_home(dir: &Path, stamp: &str, plan: Option<&str>, used: u32) {
        let day = dir.join("sessions").join(local_time(NOW, c"%Y/%m/%d"));
        fs::create_dir_all(&day).unwrap();
        let plan = plan.map_or(String::new(), |p| format!(r#""plan_type":"{p}","#));
        let line = format!(
            r#"{{"timestamp":"{stamp}","type":"event_msg","payload":{{"type":"token_count","rate_limits":{{"limit_id":"codex",{plan}"primary":{{"used_percent":{used}.0,"window_minutes":300,"resets_at":{r}}},"secondary":{{"used_percent":1.0,"window_minutes":10080,"resets_at":{r2}}}}}}}}}"#,
            r = NOW + 3600,
            r2 = NOW + 86_400
        );
        fs::write(day.join("rollout-a.jsonl"), line + "\n").unwrap();
    }

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("herdr-codex-homes-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn home(path: &Path, active: bool) -> Home {
        Home {
            path: path.to_path_buf(),
            active,
        }
    }

    fn used(usage: &ProviderUsage) -> f64 {
        usage.windows[0].left_pct.map_or(-1.0, f64::from)
    }

    #[test]
    fn two_homes_with_different_times_pick_the_newer() {
        let (a, b) = (temp("newer-a"), temp("newer-b"));
        write_home(&a, "2026-10-15T10:00:00.000Z", None, 20);
        write_home(&b, "2026-10-15T11:00:00.000Z", None, 70);
        let got = read_homes(&[home(&a, false), home(&b, false)], NOW);
        let swapped = read_homes(&[home(&b, false), home(&a, false)], NOW);
        assert_eq!(got.seen_at, swapped.seen_at);
        assert_eq!(used(&got), 30.0);
    }

    #[test]
    fn a_missing_home_is_skipped_and_the_configured_home_works_alone() {
        let a = temp("alone");
        write_home(&a, "2026-10-15T10:00:00.000Z", None, 20);
        let missing = a.join("nope");
        assert_eq!(
            used(&read_homes(&[home(&missing, false), home(&a, false)], NOW)),
            80.0
        );
        assert_eq!(used(&read_homes(&[home(&a, false)], NOW)), 80.0);
        assert_eq!(
            read_homes(&[home(&missing, false)], NOW).problem,
            Some(UsageProblem::NoData)
        );
    }

    #[test]
    fn different_plans_are_never_mixed() {
        let (cfg, other) = (temp("plan-cfg"), temp("plan-other"));
        write_home(&cfg, "2026-10-15T10:00:00.000Z", Some("plus"), 20);
        write_home(&other, "2026-10-15T11:00:00.000Z", Some("pro"), 70);
        // No running process: the configured home's account wins though older.
        let got = read_homes(&[home(&cfg, false), home(&other, false)], NOW);
        assert_eq!(used(&got), 80.0);
        // The home of a running Codex process decides when there is one.
        let got = read_homes(&[home(&cfg, false), home(&other, true)], NOW);
        assert_eq!(used(&got), 30.0);
    }
}

use super::{
    local_time, timestamp, unknown, window, Provider, ProviderUsage, UsageProblem, WindowKind,
};
use serde::Deserialize;
use serde_json::Value;
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::SystemTime,
};

fn file_entries(folder: &Path) -> Vec<(SystemTime, PathBuf)> {
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
    paths
}
fn files(folder: &Path) -> Vec<PathBuf> {
    file_entries(folder)
        .into_iter()
        .take(5)
        .map(|(_, path)| path)
        .collect()
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
fn older_folders(root: &Path, before: &Path) -> Vec<PathBuf> {
    let Ok(years) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut folders = Vec::new();
    for year in years.flatten().filter(|e| {
        e.file_name().to_string_lossy().len() == 4 && e.file_type().is_ok_and(|t| t.is_dir())
    }) {
        let Ok(months) = fs::read_dir(year.path()) else {
            continue;
        };
        for month in months
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        {
            let Ok(days) = fs::read_dir(month.path()) else {
                continue;
            };
            folders.extend(days.flatten().filter_map(|day| {
                let path = day.path();
                (path < before && day.file_type().is_ok_and(|t| t.is_dir())).then_some(path)
            }));
        }
    }
    folders.sort_by_key(|path| std::cmp::Reverse(path.clone()));
    folders
}
fn active_files(root: &Path, today: &Path) -> Vec<PathBuf> {
    // Active rollouts can keep appending to a file under the day they started.
    // Inspect metadata across dated folders, then read tails from at most five files.
    let mut entries = file_entries(today);
    for folder in older_folders(root, today) {
        // A file outside a folder's newest five cannot make the global newest five.
        entries.extend(file_entries(&folder).into_iter().take(5));
    }
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    entries.into_iter().take(5).map(|(_, path)| path).collect()
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
            if event.payload.rate_limits.plan_type.as_deref() == Some("openrouter")
                && event.payload.rate_limits.primary.is_null()
                && event.payload.rate_limits.secondary.is_null()
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
/// Read at most five file tails per home. Inactive homes keep the inexpensive
/// today/previous-day lookup; active homes also consider older rollout folders
/// because a live conversation can keep appending to its start-day file.
fn reading_in(home: &Path, now: u64, active: bool) -> Option<Reading> {
    let root = home.join("sessions");
    let today = root.join(local_time(now, c"%Y/%m/%d"));
    if active {
        return latest(&active_files(&root, &today));
    }
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
        if let Some(reading) = reading_in(&home.path, now, home.active) {
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
    build(reading_in(home, now, false), now)
}

fn build(best: Option<Reading>, now: u64) -> ProviderUsage {
    let Some(reading) = best else {
        return unknown(Provider::Codex, UsageProblem::NoData);
    };
    if reading.limits.plan_type.as_deref() == Some("openrouter")
        && reading.limits.primary.is_null()
        && reading.limits.secondary.is_null()
    {
        return unknown(Provider::Codex, UsageProblem::NoData);
    }
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
    use std::sync::atomic::{AtomicU64, Ordering};

    const NOW: u64 = 1_792_000_000;

    fn write_reading(
        dir: &Path,
        folder_time: u64,
        stamp: &str,
        plan: Option<&str>,
        primary: serde_json::Value,
        secondary: serde_json::Value,
    ) {
        let day = dir
            .join("sessions")
            .join(local_time(folder_time, c"%Y/%m/%d"));
        fs::create_dir_all(&day).unwrap();
        let line = serde_json::json!({
            "timestamp": stamp,
            "type": "event_msg",
            "payload": {"type": "token_count", "rate_limits": {
                "limit_id": "codex", "plan_type": plan,
                "primary": primary, "secondary": secondary
            }}
        });
        fs::write(day.join("rollout-a.jsonl"), format!("{line}\n")).unwrap();
    }

    fn write_home(dir: &Path, stamp: &str, plan: Option<&str>, used: u32) {
        write_reading(
            dir,
            NOW,
            stamp,
            plan,
            serde_json::json!({"used_percent": used, "resets_at": NOW + 3600}),
            serde_json::json!({"used_percent": 1, "resets_at": NOW + 86_400}),
        );
    }

    fn temp(name: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "herdr-codex-homes-{name}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
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

    #[test]
    fn active_older_dated_rollout_beats_stale_today_reading_with_five_file_cap() {
        let path = temp("active-older-day");
        write_home(&path, "2026-10-14T16:00:00Z", Some("plus"), 81);
        let today = path.join("sessions").join(local_time(NOW, c"%Y/%m/%d"));
        for n in 0..5 {
            fs::write(today.join(format!("other-{n}.jsonl")), "no token data\n").unwrap();
        }
        write_reading(
            &path,
            NOW - 86_400,
            "2026-10-14T17:46:00Z",
            Some("plus"),
            serde_json::json!({"used_percent": 39, "resets_at": NOW + 3600}),
            serde_json::json!({"used_percent": 36, "resets_at": NOW + 86_400}),
        );

        let got = read_homes(&[home(&path, true)], NOW);
        assert_eq!(got.windows[0].left_pct, Some(61));
        assert_eq!(got.windows[1].left_pct, Some(64));
        assert_eq!(got.seen_at, timestamp("2026-10-14T17:46:00Z"));
        assert_eq!(active_files(&path.join("sessions"), &today).len(), 5);
    }

    #[test]
    fn null_openrouter_limits_do_not_override_a_real_home_reading() {
        let configured = temp("configured-plus");
        let openrouter = temp("openrouter-null");
        write_home(&configured, "2026-10-14T16:00:00Z", Some("plus"), 20);
        write_reading(
            &openrouter,
            NOW,
            "2026-10-14T17:46:00Z",
            Some("openrouter"),
            serde_json::Value::Null,
            serde_json::Value::Null,
        );

        let got = read_homes(&[home(&configured, false), home(&openrouter, true)], NOW);
        assert_eq!(got.windows[0].left_pct, Some(80));
        assert_eq!(got.windows[1].left_pct, Some(99));
        assert_eq!(got.seen_at, timestamp("2026-10-14T16:00:00Z"));
    }
}

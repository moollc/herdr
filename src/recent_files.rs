//! Durable, provenance-based list of files explicitly written by agent hooks.
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_JOURNAL_BYTES: u64 = 512 * 1024;
const MAX_JOURNAL_RECORDS: usize = 500;
const MAX_RESULTS: usize = 100;
const JOURNAL_NAME: &str = "recent-agent-files.jsonl";
const GAP_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecentFile {
    pub path: PathBuf,
    pub modified: SystemTime,
    pub agent: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RecentFilesSnapshot {
    pub root: Option<PathBuf>,
    pub files: Vec<RecentFile>,
    pub loading: bool,
    pub truncated: bool,
    pub error: Option<String>,
    pub open_error: Option<String>,
    pub selected: Option<usize>,
    pub scroll_offset: usize,
    pub keyboard_focus: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct JournalStamp {
    modified: SystemTime,
    len: u64,
}

pub(crate) struct RecentFilesWatcher {
    root: Option<PathBuf>,
    journal: PathBuf,
    stamp: Option<JournalStamp>,
    next_poll: Instant,
}

impl RecentFilesWatcher {
    pub fn new() -> Self {
        Self::with_journal_path(crate::config::state_dir().join(JOURNAL_NAME))
    }

    fn with_journal_path(journal: PathBuf) -> Self {
        Self {
            root: None,
            journal,
            stamp: None,
            next_poll: Instant::now(),
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.root.as_ref().map(|_| self.next_poll)
    }

    /// Read only Herdr's bounded activity journal; never traverse workspace paths.
    pub fn poll(
        &mut self,
        now: Instant,
        root: Option<PathBuf>,
        snapshot: &mut RecentFilesSnapshot,
    ) -> bool {
        let mut changed = false;
        if root != self.root {
            self.root = root.clone();
            if root.is_none() {
                *snapshot = RecentFilesSnapshot::default();
                self.stamp = None;
                return true;
            }
            snapshot.root = root.clone();
            snapshot.loading = true;
            changed = true;
        }
        if root.is_none() || now < self.next_poll {
            return changed;
        }
        self.next_poll = now + Duration::from_secs(1);

        let current_stamp = fs::metadata(&self.journal).ok().and_then(|metadata| {
            Some(JournalStamp {
                modified: metadata.modified().ok()?,
                len: metadata.len(),
            })
        });
        if current_stamp == self.stamp && !changed {
            return false;
        }
        self.stamp = current_stamp;
        let old_selected = snapshot
            .selected
            .and_then(|index| snapshot.files.get(index))
            .map(|file| file.path.clone());
        let (files, truncated, error) = match read_journal(&self.journal) {
            Ok(result) => result,
            Err(error) => (Vec::new(), true, Some(error)),
        };
        snapshot.files = files;
        snapshot.truncated = truncated;
        snapshot.error = error;
        snapshot.loading = false;
        snapshot.selected = old_selected
            .and_then(|path| snapshot.files.iter().position(|file| file.path == path))
            .or_else(|| {
                snapshot
                    .selected
                    .filter(|index| *index < snapshot.files.len())
            });
        if snapshot.selected.is_none() && snapshot.keyboard_focus {
            snapshot.selected = (!snapshot.files.is_empty()).then_some(0);
        }
        snapshot.scroll_offset = snapshot
            .scroll_offset
            .min(snapshot.files.len().saturating_sub(1));
        changed = true;
        changed
    }
}

fn read_journal(journal: &Path) -> Result<(Vec<RecentFile>, bool, Option<String>), String> {
    let bytes = match fs::read(journal) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), false, None));
        }
        Err(_) => return Err("Agent file activity journal is unavailable".into()),
    };
    if bytes.len() as u64 > MAX_JOURNAL_BYTES {
        return Err("Agent file activity journal exceeded its size limit".into());
    }

    let journal_absolute = fs::canonicalize(journal).ok();
    let mut by_path: HashMap<PathBuf, RecentFile> = HashMap::new();
    let mut partial = false;
    let mut record_count = 0;
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        record_count += 1;
        let Ok(record) = serde_json::from_slice::<serde_json::Value>(line) else {
            partial = true;
            continue;
        };
        partial |= record
            .get("truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if record.get("gap").is_some() {
            let Some(time_ns) = record.get("time_ns").and_then(serde_json::Value::as_u64) else {
                partial = true;
                continue;
            };
            let age = SystemTime::now()
                .duration_since(UNIX_EPOCH + Duration::from_nanos(time_ns))
                .unwrap_or_default();
            partial |= age <= GAP_MAX_AGE;
            continue;
        }
        let Some(path) = record.get("path").and_then(serde_json::Value::as_str) else {
            partial = true;
            continue;
        };
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            partial = true;
            continue;
        }
        if path == journal
            || fs::canonicalize(&path)
                .ok()
                .as_ref()
                .is_some_and(|path| journal_absolute.as_ref() == Some(path))
        {
            continue;
        }
        let Some(time_ns) = record.get("time_ns").and_then(serde_json::Value::as_u64) else {
            partial = true;
            continue;
        };
        let Some(agent) = record.get("agent").and_then(serde_json::Value::as_str) else {
            partial = true;
            continue;
        };
        if record
            .get("provenance")
            .and_then(serde_json::Value::as_str)
            .is_none()
        {
            partial = true;
            continue;
        }
        if !matches!(agent, "codex" | "cursor") || !is_regular_file(&path) {
            partial = true;
            continue;
        }
        let file = RecentFile {
            path: path.clone(),
            modified: UNIX_EPOCH + Duration::from_nanos(time_ns),
            agent: agent.to_string(),
        };
        match by_path.get(&path) {
            Some(current) if current.modified >= file.modified => {}
            _ => {
                by_path.insert(path, file);
            }
        }
    }
    if record_count > MAX_JOURNAL_RECORDS {
        partial = true;
    }
    let mut files: Vec<_> = by_path.into_values().collect();
    files.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.path.cmp(&b.path))
    });
    let truncated = partial || files.len() > MAX_RESULTS;
    files.truncate(MAX_RESULTS);
    let error = partial.then(|| "Some agent file events were incomplete or unsupported".into());
    Ok((files, truncated, error))
}

pub(crate) fn display_name(path: &Path, width: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if name.width() <= width {
        return name.into_owned();
    }
    if width == 0 {
        return String::new();
    }
    let extension = path
        .extension()
        .map(|value| format!(".{}", value.to_string_lossy()))
        .unwrap_or_default();
    let (text, keep) = (
        path.file_stem().unwrap_or_default().to_string_lossy(),
        !extension.is_empty() && extension.width() < width.saturating_sub(1),
    );
    let (source, room, suffix) = if keep {
        (&*text, width - extension.width() - 1, extension.as_str())
    } else {
        (&*name, width - 1, "")
    };
    let mut prefix = String::new();
    let mut used = 0;
    for ch in source.chars() {
        let cells = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cells > room {
            break;
        }
        prefix.push(ch);
        used += cells;
    }
    format!("{prefix}…{suffix}")
}

pub(crate) fn age(modified: SystemTime, now: SystemTime) -> String {
    let seconds = now.duration_since(modified).unwrap_or_default().as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

pub(crate) fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_file())
        .unwrap_or(false)
}

pub(crate) fn move_selection(snapshot: &mut RecentFilesSnapshot, down: bool, visible: usize) {
    let len = snapshot.files.len();
    if len == 0 {
        return;
    }
    let current = snapshot.selected.unwrap_or(0).min(len - 1);
    let next = if down {
        (current + 1).min(len - 1)
    } else {
        current.saturating_sub(1)
    };
    snapshot.selected = Some(next);
    if next < snapshot.scroll_offset {
        snapshot.scroll_offset = next;
    } else if visible > 0 && next >= snapshot.scroll_offset + visible {
        snapshot.scroll_offset = next + 1 - visible;
    }
}

pub(crate) fn selected_for_open(snapshot: &RecentFilesSnapshot) -> Option<usize> {
    snapshot
        .selected
        .filter(|index| *index < snapshot.files.len())
}

pub(crate) fn begin_keyboard_focus(
    snapshot: &mut RecentFilesSnapshot,
    viewport: Option<(u16, usize)>,
) {
    if viewport.is_some_and(|(_, visible)| visible > 0) {
        snapshot.keyboard_focus = true;
        snapshot.selected = (!snapshot.files.is_empty()).then_some(0);
        snapshot.scroll_offset = 0;
    } else {
        snapshot.keyboard_focus = false;
        snapshot.selected = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "herdr-recent-journal-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&path).expect("fixture dir");
        path
    }

    fn event(path: &Path, time_ns: u64, agent: &str) -> serde_json::Value {
        serde_json::json!({
            "path": path,
            "time_ns": time_ns,
            "agent": agent,
            "provenance": format!("{agent}.PostToolUse.fixture"),
            "event_id": format!("fixture-{time_ns}"),
        })
    }

    #[test]
    fn journal_lists_untracked_outside_root_paths_and_deduplicates_by_latest_event() {
        let base = fixture_dir("outside");
        let workspace = base.join("workspace");
        let outside = base.join("outside path").join("résumé.html");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::create_dir_all(outside.parent().unwrap()).expect("outside");
        fs::write(&outside, "tiny fixture").expect("file");
        let journal = base.join("state").join(JOURNAL_NAME);
        fs::create_dir_all(journal.parent().unwrap()).expect("state");
        let records = [
            event(&outside, 100, "cursor"),
            event(&outside, 200, "codex"),
        ];
        fs::write(
            &journal,
            records
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .expect("journal");
        let (files, truncated, error) = read_journal(&journal).expect("read journal");
        assert!(!truncated);
        assert_eq!(error, None);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, outside);
        assert_eq!(files[0].agent, "codex");
        assert_eq!(files[0].modified, UNIX_EPOCH + Duration::from_nanos(200));
    }

    #[test]
    fn another_process_change_without_a_journal_event_is_not_discovered() {
        let base = fixture_dir("no-event");
        let output = base.join("untracked.txt");
        fs::write(&output, "process-only output").expect("file");
        let (files, _, _) = read_journal(&base.join(JOURNAL_NAME)).expect("empty journal");
        assert!(files.is_empty());
    }

    #[test]
    fn journal_survives_watcher_restart_and_ignores_its_own_path() {
        let base = fixture_dir("restart");
        let output = base.join("result.txt");
        fs::write(&output, "text").expect("output");
        let journal = base.join(JOURNAL_NAME);
        let first = event(&output, 123, "cursor").to_string();
        let self_event = event(&journal, 124, "codex").to_string();
        fs::write(&journal, format!("{first}\n{self_event}\n")).expect("journal");
        let (files, _, error) = read_journal(&journal).expect("first read");
        assert_eq!(error, None);
        assert_eq!(files.len(), 1);
        let (after_restart, _, _) = read_journal(&journal).expect("restarted read");
        assert_eq!(after_restart, files);
    }

    #[test]
    fn malformed_partial_state_is_reported_instead_of_claimed_complete() {
        let base = fixture_dir("partial");
        let journal = base.join(JOURNAL_NAME);
        let gap = serde_json::json!({
            "gap": "unverified or failed write event",
            "time_ns": 1,
            "agent": "codex",
            "provenance": "codex.PostToolUse.Bash.unverified",
            "event_id": "gap:fixture",
        });
        fs::write(&journal, format!("{{bad json}}\n{}\n", gap)).expect("journal");
        let (files, truncated, error) = read_journal(&journal).expect("read partial state");
        assert!(files.is_empty());
        assert!(truncated);
        assert!(error.is_some());
    }

    #[test]
    fn gap_indicators_expire_after_one_day() {
        let base = fixture_dir("expired-gap");
        let journal = base.join(JOURNAL_NAME);
        let old_time_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .saturating_sub(GAP_MAX_AGE + Duration::from_secs(1))
            .as_nanos() as u64;
        let gap = serde_json::json!({
            "gap": "unverified or failed write event",
            "time_ns": old_time_ns,
            "agent": "codex",
            "provenance": "codex.PostToolUse.Bash.unverified",
            "event_id": "gap:expired",
        });
        fs::write(&journal, gap.to_string()).expect("journal");
        let (files, truncated, error) = read_journal(&journal).expect("read expired gap");
        assert!(files.is_empty());
        assert!(!truncated);
        assert_eq!(error, None);
    }

    #[test]
    fn hidden_viewport_cannot_receive_keyboard_focus() {
        let mut snapshot = RecentFilesSnapshot {
            files: vec![RecentFile {
                path: PathBuf::from("/tmp/not-visible.txt"),
                modified: UNIX_EPOCH,
                agent: "codex".into(),
            }],
            ..RecentFilesSnapshot::default()
        };
        begin_keyboard_focus(&mut snapshot, None);
        assert!(!snapshot.keyboard_focus);
        assert_eq!(selected_for_open(&snapshot), None);
    }

    #[test]
    fn updated_file_moves_to_top_once_and_keyboard_selection_targets_enter() {
        let base = fixture_dir("order");
        let older = base.join("older report.txt");
        let current = base.join("current report.html");
        fs::write(&older, "old").expect("old file");
        fs::write(&current, "current").expect("current file");
        let journal = base.join(JOURNAL_NAME);
        let records = [
            event(&older, 100, "cursor"),
            event(&current, 200, "codex"),
            event(&older, 300, "codex"),
        ];
        fs::write(
            &journal,
            records
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .expect("journal");
        let (files, _, _) = read_journal(&journal).expect("read journal");
        assert_eq!(files[0].path, older);
        assert_eq!(files.iter().filter(|file| file.path == older).count(), 1);
        let mut snapshot = RecentFilesSnapshot {
            files,
            selected: Some(0),
            keyboard_focus: true,
            ..RecentFilesSnapshot::default()
        };
        move_selection(&mut snapshot, true, 1);
        assert_eq!(selected_for_open(&snapshot), Some(1));
        assert!(is_regular_file(&older));
        assert!(!is_regular_file(&base.join("deleted before open.txt")));
        assert!(display_name(Path::new("long résumé report.webp"), 14).ends_with(".webp"));
    }
}

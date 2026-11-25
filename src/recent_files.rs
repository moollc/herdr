//! Recent filesystem activity, enriched by the durable provider hook journal.
mod fs_watch;
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
    next_gap_refresh: Option<Instant>,
    next_poll: Instant,
    filesystem: fs_watch::FileSystemWatcher,
    journal_files: Vec<RecentFile>,
    journal_truncated: bool,
    journal_error: Option<String>,
}

impl RecentFilesWatcher {
    pub fn new() -> Self {
        Self::with_journal_path(crate::config::state_dir().join(JOURNAL_NAME))
    }

    fn with_journal_path(journal: PathBuf) -> Self {
        Self {
            root: None,
            filesystem: fs_watch::FileSystemWatcher::new(
                journal.parent().unwrap_or(Path::new(".")).into(),
            ),
            journal_files: Vec::new(),
            journal_truncated: false,
            journal_error: None,
            journal,
            stamp: None,
            next_gap_refresh: None,
            next_poll: Instant::now(),
        }
    }

    pub fn set_wake(&mut self, wake: impl Fn() + Send + Sync + 'static) {
        self.filesystem.set_wake(wake);
    }

    pub fn set_extra_roots(&mut self, paths: &[String]) {
        self.filesystem.set_extra(paths);
    }

    pub fn sync_roots(
        &mut self,
        paths: impl IntoIterator<Item = PathBuf>,
        processes: impl IntoIterator<Item = u32>,
    ) {
        self.filesystem.sync(paths, processes);
    }

    pub fn deadline(&self) -> Option<Instant> {
        (self.root.is_some() || self.filesystem.active()).then_some(self.next_poll)
    }

    /// Merge worker notifications with the bounded hook journal.
    pub fn poll(
        &mut self,
        now: Instant,
        root: Option<PathBuf>,
        snapshot: &mut RecentFilesSnapshot,
    ) -> bool {
        self.poll_at(now, SystemTime::now(), root, snapshot)
    }

    fn poll_at(
        &mut self,
        now: Instant,
        wall_now: SystemTime,
        root: Option<PathBuf>,
        snapshot: &mut RecentFilesSnapshot,
    ) -> bool {
        let previous_snapshot = snapshot.clone();
        let mut changed = false;
        if root != self.root {
            self.root = root.clone();
            if root.is_none() {
                *snapshot = RecentFilesSnapshot::default();
                self.stamp = None;
                self.next_gap_refresh = None;
                return *snapshot != previous_snapshot;
            }
            snapshot.root = root.clone();
            snapshot.files.clear();
            snapshot.selected = None;
            snapshot.scroll_offset = 0;
            snapshot.truncated = false;
            snapshot.error = None;
            snapshot.loading = true;
            changed = true;
        }
        let filesystem_changed = self.filesystem.take_changed();
        if now < self.next_poll && !changed && !filesystem_changed {
            return changed;
        }
        // Journal ages and edits keep their original idle cadence. Native activity
        // wakes the existing render loop and bypasses this deadline when changed.
        self.next_poll = now + Duration::from_secs(1);
        if root.is_none() {
            return changed;
        }

        let current_stamp = fs::metadata(&self.journal).ok().and_then(|metadata| {
            Some(JournalStamp {
                modified: metadata.modified().ok()?,
                len: metadata.len(),
            })
        });
        let old_selected = snapshot
            .selected
            .and_then(|index| snapshot.files.get(index))
            .map(|file| file.path.clone());
        if current_stamp != self.stamp
            || changed
            || self
                .next_gap_refresh
                .is_some_and(|deadline| now >= deadline)
        {
            self.stamp = current_stamp;
            let (files, truncated, error, next_gap_expiry) =
                match read_journal_details(&self.journal, wall_now) {
                    Ok(result) => result,
                    Err(error) => (Vec::new(), true, Some(error), None),
                };
            self.next_gap_refresh = next_gap_expiry
                .map(|expiry| now + expiry.duration_since(wall_now).unwrap_or_default());
            self.journal_files = files;
            self.journal_truncated = truncated;
            self.journal_error = error;
        }
        snapshot.files = merge_activity(&self.filesystem.activity.files, &self.journal_files);
        snapshot.truncated = self.journal_truncated
            || self.filesystem.activity.truncated
            || self.filesystem.activity.error.is_some()
            || snapshot.files.len() > MAX_RESULTS;
        snapshot.files.truncate(MAX_RESULTS);
        snapshot.error = match (&self.journal_error, &self.filesystem.activity.error) {
            // Lost hook data must stay visible before a potentially long watcher warning.
            (Some(journal), Some(watcher)) => Some(format!("{journal}\n{watcher}")),
            (Some(error), None) | (None, Some(error)) => Some(error.clone()),
            (None, None) => None,
        };
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
        changed = *snapshot != previous_snapshot;
        changed
    }
}

/// Filesystem activity never claims a writer. A nearby explicit hook can supply a tag.
fn merge_activity(filesystem: &[RecentFile], journal: &[RecentFile]) -> Vec<RecentFile> {
    let mut files = filesystem.to_vec();
    for hook in journal {
        if let Some(file) = files.iter_mut().find(|file| file.path == hook.path) {
            let distance = file
                .modified
                .duration_since(hook.modified)
                .or_else(|_| hook.modified.duration_since(file.modified))
                .unwrap_or_default();
            if distance <= Duration::from_secs(5) {
                file.agent = hook.agent.clone();
            }
            file.modified = file.modified.max(hook.modified);
        } else {
            files.push(hook.clone());
        }
    }
    // Stable sort retains notification order when timestamps tie.
    files.sort_by_key(|file| std::cmp::Reverse(file.modified));
    files
}

#[cfg(test)]
fn read_journal(journal: &Path) -> Result<(Vec<RecentFile>, bool, Option<String>), String> {
    read_journal_at(journal, SystemTime::now())
}

#[cfg(test)]
fn read_journal_at(
    journal: &Path,
    wall_now: SystemTime,
) -> Result<(Vec<RecentFile>, bool, Option<String>), String> {
    let (files, truncated, error, _) = read_journal_details(journal, wall_now)?;
    Ok((files, truncated, error))
}

type JournalReadDetails = (Vec<RecentFile>, bool, Option<String>, Option<SystemTime>);

fn read_journal_details(
    journal: &Path,
    wall_now: SystemTime,
) -> Result<JournalReadDetails, String> {
    let bytes =
        match crate::platform::read_regular_file_limited(journal, MAX_JOURNAL_BYTES as usize) {
            Ok(crate::platform::LimitedRead::Complete(bytes)) => bytes,
            Ok(crate::platform::LimitedRead::Empty) => Vec::new(),
            Ok(crate::platform::LimitedRead::Oversized) => {
                return Err("Agent file activity journal exceeded its size limit".into());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Vec::new(), false, None, None));
            }
            Err(_) => return Err("Agent file activity journal is unavailable".into()),
        };

    let journal_absolute = fs::canonicalize(journal).ok();
    let mut by_path: HashMap<PathBuf, RecentFile> = HashMap::new();
    let mut partial = false;
    let mut next_gap_expiry: Option<SystemTime> = None;
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
            let age = wall_now
                .duration_since(UNIX_EPOCH + Duration::from_nanos(time_ns))
                .unwrap_or_default();
            if age <= GAP_MAX_AGE {
                partial = true;
                let expiry = UNIX_EPOCH
                    + Duration::from_nanos(time_ns)
                    + GAP_MAX_AGE
                    + Duration::from_secs(1);
                next_gap_expiry =
                    Some(next_gap_expiry.map_or(expiry, |current| current.min(expiry)));
            }
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
        let path = fs::canonicalize(&path).unwrap_or(path);
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
    Ok((files, truncated, error, next_gap_expiry))
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
        fs::canonicalize(path).expect("canonical fixture dir")
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
    fn watcher_reads_unchanged_journal_on_root_switch_before_poll_deadline() {
        let base = fixture_dir("watcher-root-switch-journal");
        let first_root = base.join("first");
        let second_root = base.join("second");
        fs::create_dir_all(&first_root).expect("first root");
        fs::create_dir_all(&second_root).expect("second root");
        let journal = base.join("state").join(JOURNAL_NAME);
        fs::create_dir_all(journal.parent().expect("state parent")).expect("state");
        fs::write(&journal, "").expect("empty journal");

        let mut watcher = RecentFilesWatcher::with_journal_path(journal);
        let mut snapshot = RecentFilesSnapshot::default();
        let monotonic_start = Instant::now();
        let wall_start = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert!(watcher.poll_at(monotonic_start, wall_start, Some(first_root), &mut snapshot));
        assert!(!snapshot.loading);

        assert!(watcher.poll_at(
            monotonic_start + Duration::from_millis(100),
            wall_start + Duration::from_millis(100),
            Some(second_root.clone()),
            &mut snapshot
        ));
        assert_eq!(snapshot.root, Some(second_root.clone()));
        assert!(!snapshot.loading);

        assert!(!watcher.poll_at(
            monotonic_start + Duration::from_secs(1),
            wall_start + Duration::from_secs(1),
            Some(second_root),
            &mut snapshot
        ));
        assert!(!snapshot.loading);
    }

    #[test]
    fn watcher_clears_stale_files_when_root_switches_without_a_journal() {
        let base = fixture_dir("watcher-root-switch-no-journal");
        let first_root = base.join("first");
        let second_root = base.join("second");
        fs::create_dir_all(&first_root).expect("first root");
        fs::create_dir_all(&second_root).expect("second root");
        let journal = base.join("missing-state").join(JOURNAL_NAME);
        let mut watcher = RecentFilesWatcher::with_journal_path(journal);
        let mut snapshot = RecentFilesSnapshot::default();
        let monotonic_start = Instant::now();
        let wall_start = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert!(watcher.poll_at(monotonic_start, wall_start, Some(first_root), &mut snapshot));
        assert!(!snapshot.loading);

        snapshot.files.push(RecentFile {
            path: base.join("stale-from-previous-root.txt"),
            modified: wall_start,
            agent: "codex".into(),
        });
        snapshot.selected = Some(0);
        snapshot.scroll_offset = 1;
        assert!(watcher.poll_at(
            monotonic_start + Duration::from_millis(100),
            wall_start + Duration::from_millis(100),
            Some(second_root.clone()),
            &mut snapshot
        ));

        assert_eq!(snapshot.root, Some(second_root));
        assert!(snapshot.files.is_empty());
        assert_eq!(snapshot.selected, None);
        assert_eq!(snapshot.scroll_offset, 0);
        assert!(!snapshot.loading);
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
    fn watcher_refreshes_expired_gap_without_journal_write() {
        let base = fixture_dir("watcher-expired-gap");
        let journal = base.join(JOURNAL_NAME);
        let observed_at = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let gap = serde_json::json!({
            "gap": "unverified or failed write event",
            "time_ns": observed_at.duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64,
            "agent": "codex",
            "provenance": "codex.PostToolUse.Bash.unverified",
            "event_id": "gap:watcher-expiry",
        });
        fs::write(&journal, gap.to_string()).expect("journal");
        let original_stamp = fs::metadata(&journal)
            .expect("journal metadata")
            .modified()
            .expect("journal modified time");
        let mut watcher = RecentFilesWatcher::with_journal_path(journal.clone());
        let mut snapshot = RecentFilesSnapshot::default();
        let monotonic_start = Instant::now();
        assert!(watcher.poll_at(
            monotonic_start,
            observed_at,
            Some(base.clone()),
            &mut snapshot
        ));
        assert!(snapshot.truncated);
        assert!(snapshot.error.is_some());
        assert_eq!(
            watcher.next_gap_refresh,
            Some(monotonic_start + GAP_MAX_AGE + Duration::from_secs(1))
        );

        assert!(!watcher.poll_at(
            monotonic_start + Duration::from_secs(1),
            observed_at + Duration::from_secs(1),
            Some(base.clone()),
            &mut snapshot,
        ));

        assert!(watcher.poll_at(
            monotonic_start + GAP_MAX_AGE + Duration::from_secs(2),
            observed_at + GAP_MAX_AGE + Duration::from_secs(2),
            Some(base),
            &mut snapshot,
        ));
        assert!(!snapshot.truncated);
        assert_eq!(snapshot.error, None);
        assert_eq!(
            fs::metadata(&journal)
                .expect("journal remains")
                .modified()
                .expect("journal modified time remains"),
            original_stamp,
        );
    }

    #[test]
    fn non_gap_partial_does_not_refresh_an_unchanged_journal_each_poll() {
        let base = fixture_dir("watcher-non-gap-partial");
        let journal = base.join(JOURNAL_NAME);
        fs::write(&journal, b"{bad json}\n").expect("malformed journal");
        let mut watcher = RecentFilesWatcher::with_journal_path(journal);
        let mut snapshot = RecentFilesSnapshot::default();
        let monotonic_start = Instant::now();
        let wall_start = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert!(watcher.poll_at(
            monotonic_start,
            wall_start,
            Some(base.clone()),
            &mut snapshot
        ));
        assert!(snapshot.truncated);
        assert_eq!(watcher.next_gap_refresh, None);

        let previous = snapshot.clone();
        assert!(!watcher.poll_at(
            monotonic_start + Duration::from_secs(1),
            wall_start + Duration::from_secs(1),
            Some(base),
            &mut snapshot,
        ));
        assert_eq!(snapshot, previous);
    }

    #[test]
    fn unchanged_snapshot_does_not_emit_changed_when_journal_stamp_changes() {
        let base = fixture_dir("watcher-equal-snapshot");
        let journal = base.join(JOURNAL_NAME);
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let gap = serde_json::json!({
            "gap": "unverified or failed write event",
            "time_ns": now.duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64,
            "agent": "codex",
            "provenance": "codex.PostToolUse.Bash.unverified",
            "event_id": "gap:snapshot-equal",
        });
        fs::write(&journal, gap.to_string()).expect("journal");
        let mut watcher = RecentFilesWatcher::with_journal_path(journal.clone());
        let mut snapshot = RecentFilesSnapshot::default();
        let monotonic_start = Instant::now();
        assert!(watcher.poll_at(monotonic_start, now, Some(base.clone()), &mut snapshot));
        let previous = snapshot.clone();
        fs::write(&journal, format!("{}\n", gap))
            .expect("change journal stamp without changing records");
        assert!(!watcher.poll_at(
            monotonic_start + Duration::from_secs(1),
            now + Duration::from_secs(1),
            Some(base),
            &mut snapshot,
        ));
        assert_eq!(snapshot, previous);
    }

    #[test]
    fn filesystem_merge_uses_nearby_hooks_without_claiming_old_authorship() {
        let time = UNIX_EPOCH + Duration::from_secs(100);
        let file = |name: &str, seconds: u64, agent: &str| RecentFile {
            path: PathBuf::from(name),
            modified: time + Duration::from_secs(seconds),
            agent: agent.into(),
        };
        let filesystem = vec![file("/a", 10, ""), file("/b", 8, ""), file("/old", 7, "")];
        let journal = vec![
            file("/a", 9, "codex"),
            file("/old", 0, "cursor"),
            file("/outside", 11, "cursor"),
        ];
        let files = merge_activity(&filesystem, &journal);
        assert_eq!(files.len(), 4);
        assert_eq!(files[0].path, PathBuf::from("/outside"));
        assert_eq!(files[1].agent, "codex");
        assert_eq!(files[1].modified, time + Duration::from_secs(10));
        assert!(files[2].agent.is_empty());
        assert!(files[3].agent.is_empty());
    }

    #[test]
    fn merge_cap_and_partial_error_keep_rows_and_selection() {
        let base = fixture_dir("merged-cap");
        let journal = base.join("state").join(JOURNAL_NAME);
        let mut watcher = RecentFilesWatcher::with_journal_path(journal);
        let mut snapshot = RecentFilesSnapshot::default();
        watcher.filesystem.activity.files = (0..MAX_RESULTS + 5)
            .map(|i| RecentFile {
                path: base.join(i.to_string()),
                modified: UNIX_EPOCH + Duration::from_secs(i as u64),
                agent: String::new(),
            })
            .collect();
        watcher.filesystem.activity.error = Some("File watcher partial".into());
        assert!(watcher.poll(Instant::now(), Some(base), &mut snapshot));
        assert_eq!(snapshot.files.len(), MAX_RESULTS);
        assert!(snapshot.truncated);
        assert_eq!(snapshot.error.as_deref(), Some("File watcher partial"));
    }

    fn error_snapshot(journal: Option<&str>, watcher_error: Option<&str>) -> RecentFilesSnapshot {
        let base = fixture_dir("error-selection");
        let journal_path = base.join(JOURNAL_NAME);
        if let Some(contents) = journal {
            fs::write(&journal_path, contents).expect("journal");
        }
        let mut watcher = RecentFilesWatcher::with_journal_path(journal_path);
        watcher.filesystem.activity.error = watcher_error.map(str::to_owned);
        let mut snapshot = RecentFilesSnapshot::default();
        watcher.poll(Instant::now(), Some(base), &mut snapshot);
        snapshot
    }

    #[test]
    fn error_selection_both_diagnostics_keep_journal_first() {
        let snapshot = error_snapshot(
            Some("{corrupt}\n"),
            Some("File watcher partial, coverage may be incomplete"),
        );
        assert_eq!(
            snapshot.error.as_deref(),
            Some("Some agent file events were incomplete or unsupported\nFile watcher partial, coverage may be incomplete")
        );
        assert!(snapshot.truncated);
    }

    #[test]
    fn error_selection_journal_only_is_unchanged() {
        let snapshot = error_snapshot(Some("{corrupt}\n"), None);
        assert_eq!(
            snapshot.error.as_deref(),
            Some("Some agent file events were incomplete or unsupported")
        );
        assert!(snapshot.truncated);
    }

    #[test]
    fn error_selection_watcher_only_is_unchanged() {
        let snapshot = error_snapshot(None, Some("File watcher partial"));
        assert_eq!(snapshot.error.as_deref(), Some("File watcher partial"));
        assert!(snapshot.truncated);
    }

    #[test]
    fn error_selection_neither_diagnostic_stays_empty() {
        let snapshot = error_snapshot(None, None);
        assert_eq!(snapshot.error, None);
        assert!(!snapshot.truncated);
    }

    #[test]
    fn error_selection_unavailable_journal_precedes_watcher_warning() {
        let base = fixture_dir("unavailable-error-selection");
        let journal = base.join(JOURNAL_NAME);
        fs::create_dir(&journal).expect("nonregular journal");
        let mut watcher = RecentFilesWatcher::with_journal_path(journal);
        watcher.filesystem.activity.error = Some("File watcher partial".into());
        let mut snapshot = RecentFilesSnapshot::default();
        watcher.poll(Instant::now(), Some(base), &mut snapshot);
        assert_eq!(
            snapshot.error.as_deref(),
            Some("Agent file activity journal is unavailable\nFile watcher partial")
        );
        assert!(snapshot.truncated);
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

    #[test]
    fn safety_regression_idle_journal_keeps_one_second_deadline() {
        let base = fixture_dir("idle-cadence");
        let mut watcher = RecentFilesWatcher::with_journal_path(base.join(JOURNAL_NAME));
        let mut snapshot = RecentFilesSnapshot::default();
        let now = Instant::now();
        watcher.poll_at(now, SystemTime::now(), Some(base), &mut snapshot);
        assert_eq!(watcher.deadline(), Some(now + Duration::from_secs(1)));
    }
}

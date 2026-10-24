//! Bounded background indexing for user-facing files in the selected space.
use std::{
    fs,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, TryRecvError},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

const RESCAN: Duration = Duration::from_secs(20);
const MAX_DIRECTORIES: usize = 2_000;
const MAX_FILES: usize = 20_000;
const MAX_DEPTH: usize = 10;
const MAX_RESULTS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecentFile {
    pub path: PathBuf,
    pub modified: SystemTime,
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

struct ScanResult {
    root: PathBuf,
    files: Vec<RecentFile>,
    truncated: bool,
    error: Option<String>,
}

pub(crate) struct RecentFilesWatcher {
    root: Option<PathBuf>,
    next_scan: Option<Instant>,
    next_poll: Instant,
    result_rx: Option<Receiver<ScanResult>>,
    cancel: Option<Arc<AtomicBool>>,
    in_flight: bool,
}

impl RecentFilesWatcher {
    pub fn new() -> Self {
        Self {
            root: None,
            next_scan: None,
            next_poll: Instant::now(),
            result_rx: None,
            cancel: None,
            in_flight: false,
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.root.as_ref().map(|_| self.next_poll)
    }

    pub fn poll(
        &mut self,
        now: Instant,
        root: Option<PathBuf>,
        snapshot: &mut RecentFilesSnapshot,
    ) -> bool {
        let mut changed = false;
        if root != self.root {
            if let Some(cancel) = self.cancel.take() {
                cancel.store(true, Ordering::Relaxed);
            }
            self.root = root.clone();
            self.result_rx = None;
            self.in_flight = false;
            self.next_scan = root.as_ref().map(|_| now);
            *snapshot = RecentFilesSnapshot {
                root: root.clone(),
                loading: root.is_some(),
                ..RecentFilesSnapshot::default()
            };
            changed = true;
        }
        if let Some(receiver) = &self.result_rx {
            match receiver.try_recv() {
                Ok(result) => {
                    self.result_rx = None;
                    self.in_flight = false;
                    self.next_scan = Some(now + RESCAN);
                    if self.root.as_ref() == Some(&result.root) {
                        snapshot.files = result.files;
                        snapshot.truncated = result.truncated;
                        snapshot.error = result.error;
                        snapshot.loading = false;
                        snapshot.selected = snapshot
                            .selected
                            .filter(|index| *index < snapshot.files.len());
                        if snapshot.selected.is_none() && snapshot.keyboard_focus {
                            snapshot.selected = (!snapshot.files.is_empty()).then_some(0);
                        }
                        snapshot.scroll_offset = snapshot
                            .scroll_offset
                            .min(snapshot.files.len().saturating_sub(1));
                        changed = true;
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    self.result_rx = None;
                    self.in_flight = false;
                    self.next_scan = Some(now + RESCAN);
                    snapshot.loading = false;
                    snapshot.error = Some("Recent files could not be indexed".into());
                    changed = true;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.root.is_some()
            && !self.in_flight
            && self.next_scan.is_some_and(|deadline| now >= deadline)
        {
            self.start_scan();
        }
        self.next_poll = now + Duration::from_secs(1);
        changed
    }

    fn start_scan(&mut self) {
        let Some(root) = self.root.clone() else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        thread::spawn(move || {
            let _ = tx.send(scan_root_cancellable(&root, &worker_cancel));
        });
        self.result_rx = Some(rx);
        self.cancel = Some(cancel);
        self.in_flight = true;
        self.next_scan = None;
    }
}

fn ignored_directory(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        ".git"
            | ".herdr"
            | ".codex"
            | ".claude"
            | ".config"
            | ".secrets"
            | ".credentials"
            | ".ssh"
            | ".aws"
            | ".kube"
            | ".npm"
            | ".cargo"
            | "target"
            | "node_modules"
            | "dist"
            | "build"
            | "out"
            | "coverage"
            | "logs"
            | "log"
            | "cache"
            | ".cache"
            | ".next"
            | ".turbo"
            | "vendor"
            | "config"
            | "configs"
            | "configuration"
            | "auth"
            | "secrets"
            | "credentials"
            | "tmp"
            | "temp"
    )
}

fn ignored_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with(".env")
        || lower == ".npmrc"
        || lower == ".pypirc"
        || ["credential", "secret"]
            .iter()
            .any(|part| lower.contains(part))
        || lower == "auth"
        || lower.starts_with("auth.")
        || lower.starts_with("auth_")
        || lower == "token"
        || lower.starts_with("token.")
        || lower.starts_with("token_")
        || lower.starts_with("id_rsa")
        || lower.starts_with("id_ed25519")
        || [".pem", ".key", ".p12", ".pfx", ".mobileprovision"]
            .iter()
            .any(|ext| lower.ends_with(ext))
        || matches!(
            lower.as_str(),
            "config.toml" | "settings.json" | "credentials.json" | "secrets.json"
        )
}

#[cfg(test)]
fn scan_root(root: &Path) -> ScanResult {
    scan_root_cancellable(root, &AtomicBool::new(false))
}

fn scan_root_cancellable(root: &Path, cancel: &AtomicBool) -> ScanResult {
    let mut result = ScanResult {
        root: root.to_path_buf(),
        files: Vec::new(),
        truncated: false,
        error: None,
    };
    match fs::metadata(root) {
        Ok(metadata) if metadata.is_dir() => {}
        _ => {
            result.error = Some("Project folder is unavailable".into());
            return result;
        }
    }
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    let (mut dirs, mut files) = (0usize, 0usize);
    while let Some((directory, depth)) = pending.pop() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        dirs += 1;
        if dirs > MAX_DIRECTORIES {
            result.truncated = true;
            break;
        }
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) if directory == root => {
                result.error = Some("Project folder is unavailable".into());
                return result;
            }
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if !ignored_directory(&name) {
                    if depth < MAX_DEPTH {
                        pending.push((path, depth + 1));
                    } else {
                        result.truncated = true;
                    }
                }
            } else if kind.is_file() && !ignored_file(&name) {
                files += 1;
                if files > MAX_FILES {
                    result.truncated = true;
                    pending.clear();
                    break;
                }
                if let Ok(metadata) = entry.metadata() {
                    if let Ok(modified) = metadata.modified() {
                        result.files.push(RecentFile { path, modified });
                    }
                }
            }
        }
    }
    result.files.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.path.cmp(&b.path))
    });
    result.truncated |= result.files.len() > MAX_RESULTS;
    result.files.truncate(MAX_RESULTS);
    result
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_untracked_files_newest_first_and_excludes_private_and_generated_paths() {
        let root = std::env::temp_dir().join(format!(
            "herdr-recent-files-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("reports")).expect("reports dir");
        fs::create_dir_all(root.join("target")).expect("target dir");
        fs::create_dir_all(root.join(".git")).expect("git dir");
        fs::create_dir_all(root.join("reports/assets")).expect("asset directory");
        fs::write(root.join("reports/older report.md"), "old").expect("old report");
        fs::write(root.join("reports/assets/preview.webp"), "image").expect("image output");
        thread::sleep(Duration::from_millis(15));
        fs::write(root.join("reports/最新.html"), "new").expect("new report");
        fs::write(root.join("reports/.env.local"), "secret").expect("secret file");
        fs::write(root.join("target/generated.webp"), "generated").expect("generated file");
        fs::write(root.join(".git/index"), "git").expect("git file");
        let result = scan_root(&root);
        assert_eq!(result.files.len(), 3);
        assert_eq!(
            result.files[0].path.file_name().unwrap().to_string_lossy(),
            "最新.html"
        );
        assert!(result.files[0].modified >= result.files[1].modified);
        assert!(display_name(Path::new("long report name.webp"), 12).ends_with(".webp"));
    }

    #[test]
    fn changing_workspace_discards_old_rows_and_shows_the_new_root() {
        let a = std::env::temp_dir().join(format!("herdr-recent-a-{}", std::process::id()));
        let b = std::env::temp_dir().join(format!("herdr-recent-b-{}", std::process::id()));
        fs::create_dir_all(&a).expect("first workspace");
        fs::create_dir_all(&b).expect("second workspace");
        fs::write(a.join("one.md"), "one").expect("first file");
        fs::write(b.join("two.md"), "two").expect("second file");
        let mut watcher = RecentFilesWatcher::new();
        let mut snapshot = RecentFilesSnapshot::default();
        let now = Instant::now();
        watcher.poll(now, Some(a), &mut snapshot);
        watcher.poll(
            now + Duration::from_millis(1),
            Some(b.clone()),
            &mut snapshot,
        );
        assert!(snapshot.files.is_empty());
        assert_eq!(snapshot.root, Some(b.clone()));
        let deadline = Instant::now() + Duration::from_secs(2);
        while snapshot.loading && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
            watcher.poll(Instant::now(), Some(b.clone()), &mut snapshot);
        }
        assert_eq!(snapshot.files.len(), 1);
        assert_eq!(snapshot.files[0].path, b.join("two.md"));
    }

    #[test]
    fn updated_file_moves_to_top_once_and_keyboard_selection_targets_enter() {
        let root = std::env::temp_dir().join(format!(
            "herdr-recent-update-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("project root");
        let old = root.join("older.txt");
        let current = root.join("current report.html");
        fs::write(&old, "old").expect("old file");
        thread::sleep(Duration::from_millis(15));
        fs::write(&current, "current").expect("current file");
        let first_scan = scan_root(&root);
        assert_eq!(first_scan.files[0].path, current);
        assert_eq!(
            first_scan
                .files
                .iter()
                .filter(|file| file.path == current)
                .count(),
            1
        );
        thread::sleep(Duration::from_millis(15));
        fs::write(&old, "updated").expect("update old file");
        let second_scan = scan_root(&root);
        assert_eq!(second_scan.files[0].path, old);
        assert_eq!(
            second_scan
                .files
                .iter()
                .filter(|file| file.path == old)
                .count(),
            1
        );

        let mut snapshot = RecentFilesSnapshot {
            files: second_scan.files,
            selected: Some(0),
            keyboard_focus: true,
            ..RecentFilesSnapshot::default()
        };
        move_selection(&mut snapshot, true, 1);
        assert_eq!(selected_for_open(&snapshot), Some(1));
        assert!(is_regular_file(&old));
        assert!(!is_regular_file(&root.join("deleted before open.txt")));
    }
}

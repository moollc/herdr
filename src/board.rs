//! A generic, optional file-backed board. Runtime owns the watcher; rendering
//! only sees a snapshot. No knowledge of the file's producer lives here.
use serde::Deserialize;
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct Board {
    pub title: String,
    pub updated: String,
    pub sections: Vec<Section>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct Section {
    pub heading: String,
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct Row {
    pub text: String,
    #[serde(default)]
    pub style: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BoardSnapshot {
    pub configured: bool,
    pub board: Option<Board>,
    pub error: Option<String>,
}

pub(crate) fn configured_path(config: Option<&str>) -> Option<PathBuf> {
    let env = std::env::var("HERDR_BOARD_FILE").ok();
    resolve_path(env.as_deref().or(config))
}

fn resolve_path(value: Option<&str>) -> Option<PathBuf> {
    let value = value.filter(|value| !value.trim().is_empty())?;
    if value == "~" || value.starts_with("~/") {
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join(value.strip_prefix("~/").unwrap_or("")))
    } else {
        Some(PathBuf::from(value))
    }
}

pub(crate) struct BoardWatcher {
    path: Option<PathBuf>,
    next_poll: Option<Instant>,
    stamp: Option<(SystemTime, u64)>,
    loaded: bool,
}

impl BoardWatcher {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            next_poll: None,
            stamp: None,
            loaded: false,
        }
    }

    pub fn set_path(&mut self, path: Option<PathBuf>) {
        if self.path != path {
            *self = Self::new(path);
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.path
            .as_ref()
            .map(|_| self.next_poll.unwrap_or_else(Instant::now))
    }

    pub fn poll(&mut self, now: Instant, snapshot: &mut BoardSnapshot) -> bool {
        let Some(path) = self.path.as_ref() else {
            let changed = *snapshot != BoardSnapshot::default();
            *snapshot = BoardSnapshot::default();
            return changed;
        };
        if self.next_poll.is_some_and(|deadline| now < deadline) {
            return false;
        }
        self.next_poll = Some(now + POLL_INTERVAL);
        let metadata = std::fs::metadata(path);
        let stamp = metadata
            .as_ref()
            .ok()
            .and_then(|m| m.modified().ok().map(|t| (t, m.len())));
        // Failed reads retry at the next poll, even if permissions changed but
        // mtime did not. Successful unchanged files are never read again.
        if self.loaded && snapshot.error.is_none() && stamp == self.stamp {
            return false;
        }
        self.stamp = stamp;
        self.loaded = true;
        let result = (|| {
            let metadata = metadata.map_err(|_| "Board file missing or unreadable")?;
            if !metadata.is_file() || metadata.len() > MAX_BYTES {
                return Err("Board file is not a regular JSON file (maximum 1 MiB)");
            }
            let file = std::fs::File::open(path).map_err(|_| "Board file missing or unreadable")?;
            let mut bytes = Vec::new();
            file.take(MAX_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| "Board file unreadable")?;
            if bytes.len() as u64 > MAX_BYTES {
                return Err("Board file exceeds 1 MiB");
            }
            serde_json::from_slice::<Board>(&bytes)
                .map_err(|_| "Board file contains invalid JSON or format")
        })();
        let next = match result {
            Ok(board) => BoardSnapshot {
                configured: true,
                board: Some(board),
                error: None,
            },
            Err(error) => BoardSnapshot {
                configured: true,
                board: None,
                error: Some(error.to_string()),
            },
        };
        let changed = *snapshot != next;
        *snapshot = next;
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn board_format_accepts_extensions_and_unknown_styles() {
        let board: Board = serde_json::from_str(r#"{"title":"Tasks","updated":"now","extra":true,"sections":[{"heading":"Open","rows":[{"text":"one","style":"future","extra":0},{"text":"two"}]}]}"#).unwrap();
        assert_eq!(board.sections[0].rows[0].style, "future");
        assert_eq!(board.sections[0].rows[1].style, "");
        assert!(serde_json::from_str::<Board>(r#"{"title":"Tasks","sections":[]}"#).is_err());
        let config: crate::config::Config = toml::from_str("board_file = '~/tasks.json'").unwrap();
        assert_eq!(config.board_file.as_deref(), Some("~/tasks.json"));
        assert!(resolve_path(Some("~/tasks.json")).unwrap().is_absolute());
        assert!(resolve_path(Some("")).is_none());
    }

    #[test]
    fn board_missing_broken_recovery_reload_and_throttle() {
        let path = std::env::temp_dir().join(format!(
            "board-panel-{}-{}.json",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut watcher = BoardWatcher::new(Some(path.clone()));
        let mut snapshot = BoardSnapshot::default();
        let now = Instant::now();
        assert!(watcher.poll(now, &mut snapshot));
        assert!(snapshot.configured);
        assert!(snapshot.error.as_ref().unwrap().contains("missing"));
        std::fs::write(&path, "broken").unwrap();
        assert!(!watcher.poll(now + Duration::from_secs(1), &mut snapshot));
        assert!(watcher.poll(now + POLL_INTERVAL, &mut snapshot));
        assert!(snapshot.error.as_ref().unwrap().contains("invalid"));
        std::fs::write(&path, r#"{"title":"Tasks","updated":"now","sections":[]}"#).unwrap();
        assert!(watcher.poll(now + POLL_INTERVAL * 2, &mut snapshot));
        assert!(snapshot.error.is_none());
        assert!(!watcher.poll(now + POLL_INTERVAL * 3, &mut snapshot));
        std::fs::write(
            &path,
            r#"{"title":"Changed","updated":"later","sections":[]}"#,
        )
        .unwrap();
        assert!(watcher.poll(now + POLL_INTERVAL * 4, &mut snapshot));
        assert_eq!(snapshot.board.as_ref().unwrap().title, "Changed");
        watcher.set_path(None);
        assert!(watcher.poll(now + POLL_INTERVAL * 4, &mut snapshot));
        assert!(!snapshot.configured);
        assert!(watcher.deadline().is_none());
        // No deletion: temporary test files remain in the OS temp directory.
    }
}

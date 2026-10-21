//! Generic, optional file-backed tasks. Runtime owns the watcher; rendering
//! only sees a snapshot. No knowledge of the file's producer lives here.
use serde::Deserialize;
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct Tasks {
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Mark {
    Run,
    Approve,
    Ready,
    Wait,
    Stuck,
    Done,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct Row {
    pub mark: Mark,
    pub title: String,
    pub who: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TasksSnapshot {
    pub configured: bool,
    pub tasks: Option<Tasks>,
    pub error: Option<String>,
}

pub(crate) fn configured_path(config: Option<&str>) -> Option<PathBuf> {
    let env = std::env::var("HERDR_TASKS_FILE").ok();
    resolve_path(env.as_deref().or(config), std::env::var_os("HOME"))
}

fn resolve_path(value: Option<&str>, home: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let value = value.filter(|value| !value.trim().is_empty())?;
    if value == "~" || value.starts_with("~/") {
        let home = home?;
        Some(PathBuf::from(home).join(value.strip_prefix("~/").unwrap_or("")))
    } else {
        Some(PathBuf::from(value))
    }
}

pub(crate) struct TasksWatcher {
    path: Option<PathBuf>,
    next_poll: Option<Instant>,
    stamp: Option<(SystemTime, u64)>,
    loaded: bool,
}

impl TasksWatcher {
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

    pub fn poll(&mut self, now: Instant, snapshot: &mut TasksSnapshot) -> bool {
        let Some(path) = self.path.as_ref() else {
            let changed = *snapshot != TasksSnapshot::default();
            *snapshot = TasksSnapshot::default();
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
        let missing = metadata
            .as_ref()
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound);
        let result = (|| {
            let metadata = metadata.map_err(|_| "Tasks file missing or unreadable")?;
            if !metadata.is_file() || metadata.len() > MAX_BYTES {
                return Err("Tasks file is not a regular JSON file (maximum 1 MiB)");
            }
            let file = std::fs::File::open(path).map_err(|_| "Tasks file missing or unreadable")?;
            let mut bytes = Vec::new();
            file.take(MAX_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| "Tasks file unreadable")?;
            if bytes.len() as u64 > MAX_BYTES {
                return Err("Tasks file exceeds 1 MiB");
            }
            serde_json::from_slice::<Tasks>(&bytes)
                .map_err(|_| "Tasks file contains invalid JSON or format")
        })();
        let next = match result {
            Ok(tasks) => TasksSnapshot {
                configured: true,
                tasks: Some(tasks),
                error: None,
            },
            Err(_) if missing => TasksSnapshot::default(),
            Err(error) => TasksSnapshot {
                configured: true,
                tasks: None,
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
    fn tasks_format_all_marks_and_validation() {
        let tasks:Tasks=serde_json::from_str(r#"{"rows":[{"mark":"run","title":"one","who":"owner"},{"mark":"approve","title":"two","who":"you"},{"mark":"ready","title":"three","who":""},{"mark":"wait","title":"four","who":""},{"mark":"stuck","title":"five","who":""},{"mark":"done","title":"six","who":""}],"extension":true}"#).unwrap();
        assert_eq!(tasks.rows.len(), 6);
        for invalid in [
            r#"{}"#,
            r#"{"rows":[{"mark":"other","title":"x","who":""}]}"#,
            r#"{"rows":[{"mark":"ready","title":"x"}]}"#,
        ] {
            assert!(serde_json::from_str::<Tasks>(invalid).is_err());
        }
        let config: crate::config::Config = toml::from_str("tasks_file = '~/tasks.json'").unwrap();
        assert_eq!(config.tasks_file.as_deref(), Some("~/tasks.json"));
        assert_eq!(
            resolve_path(Some("~/tasks.json"), Some("/synthetic".into())),
            Some(PathBuf::from("/synthetic/tasks.json"))
        );
        assert!(resolve_path(Some("~/tasks.json"), None).is_none());
        assert!(resolve_path(Some(""), None).is_none());
    }
    #[test]
    fn tasks_missing_broken_recovery_throttle_and_removal() {
        let path = std::env::temp_dir().join(format!(
            "tasks-column-{}-{}.json",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut watcher = TasksWatcher::new(Some(path.clone()));
        let mut snapshot = TasksSnapshot::default();
        let now = Instant::now();
        assert!(!watcher.poll(now, &mut snapshot));
        assert!(!snapshot.configured);
        std::fs::write(&path, "broken").unwrap();
        assert!(!watcher.poll(now + Duration::from_secs(1), &mut snapshot));
        assert!(watcher.poll(now + POLL_INTERVAL, &mut snapshot));
        assert!(snapshot.configured && snapshot.error.is_some());
        std::fs::write(&path, r#"{"rows":[]}"#).unwrap();
        assert!(watcher.poll(now + POLL_INTERVAL * 2, &mut snapshot));
        assert!(snapshot.tasks.is_some() && snapshot.error.is_none());
        assert!(!watcher.poll(now + POLL_INTERVAL * 3, &mut snapshot));
        std::fs::write(
            &path,
            r#"{"rows":[{"mark":"run","title":"changed","who":"owner"}]}"#,
        )
        .unwrap();
        assert!(watcher.poll(now + POLL_INTERVAL * 4, &mut snapshot));
        assert_eq!(snapshot.tasks.as_ref().unwrap().rows[0].title, "changed");
        watcher.set_path(Some(path.with_extension("absent")));
        assert!(watcher.poll(now + POLL_INTERVAL * 4, &mut snapshot));
        assert!(!snapshot.configured);
        watcher.set_path(None);
        assert!(!watcher.poll(now + POLL_INTERVAL * 4, &mut snapshot));
        assert!(watcher.deadline().is_none());
    }
}

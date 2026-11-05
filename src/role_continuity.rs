//! Append-only continuity records owned by a role scope, not a pane.
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::Digest;

pub const MEMORY_ENTRY_MAX_CHARS: usize = 2_048;
pub const HANDOFF_MAX_AGE_SECS: u64 = 6 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffPacket {
    pub task_id: String,
    pub attempt: String,
    pub candidate: String,
    pub tests: Vec<String>,
    pub output_paths: Vec<String>,
    pub open_findings: Vec<String>,
    pub cost: String,
    pub native_session: crate::overseer::SessionIdentity,
    pub next_owner: String,
    pub recorded_at_unix: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffCheck {
    pub valid: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextTransitionPlan {
    Disabled,
    MeterUnknown,
    BelowLimit,
    RequestHandoff,
    Blocked(&'static str),
    Unsupported(&'static str),
    Ready,
}

pub fn context_transition_plan(
    enabled: bool,
    usage_percent: Option<u8>,
    limit_percent: u8,
    handoff_valid: bool,
    pane_idle: bool,
    input_empty: bool,
    native_action_supported: bool,
) -> ContextTransitionPlan {
    if !enabled {
        return ContextTransitionPlan::Disabled;
    }
    let Some(usage) = usage_percent else {
        return ContextTransitionPlan::MeterUnknown;
    };
    if usage < limit_percent {
        return ContextTransitionPlan::BelowLimit;
    }
    if !handoff_valid {
        return ContextTransitionPlan::RequestHandoff;
    }
    if !pane_idle {
        return ContextTransitionPlan::Blocked("pane busy");
    }
    if !input_empty {
        return ContextTransitionPlan::Blocked("input draft present");
    }
    if !native_action_supported {
        return ContextTransitionPlan::Unsupported(
            "native compact/clear is not accepted for this adapter",
        );
    }
    ContextTransitionPlan::Ready
}

/// Effort updates are always in-place and never request context destruction.
#[cfg(test)]
pub fn effort_change_requires_clear(_old: &str, _new: &str) -> bool {
    false
}

pub fn check_handoff(
    packet: &HandoffPacket,
    now_unix: u64,
    current_session: Option<&crate::overseer::SessionIdentity>,
) -> HandoffCheck {
    let required = [
        (&packet.task_id, "task id"),
        (&packet.attempt, "attempt"),
        (&packet.candidate, "candidate"),
        (&packet.cost, "cost"),
        (&packet.next_owner, "next owner"),
    ];
    if let Some((_, field)) = required.iter().find(|(value, _)| value.trim().is_empty()) {
        return HandoffCheck {
            valid: false,
            reason: format!("missing {field}"),
        };
    }
    if packet.tests.is_empty() || packet.tests.iter().any(|value| value.trim().is_empty()) {
        return HandoffCheck {
            valid: false,
            reason: "tests must be named".into(),
        };
    }
    if packet.output_paths.is_empty()
        || packet
            .output_paths
            .iter()
            .any(|value| value.trim().is_empty())
    {
        return HandoffCheck {
            valid: false,
            reason: "output paths must be named".into(),
        };
    }
    if packet.open_findings.is_empty()
        || packet
            .open_findings
            .iter()
            .any(|value| value.trim().is_empty())
    {
        return HandoffCheck {
            valid: false,
            reason: "open findings must be named (write none when there are none)".into(),
        };
    }
    if now_unix < packet.recorded_at_unix
        || now_unix - packet.recorded_at_unix > HANDOFF_MAX_AGE_SECS
    {
        return HandoffCheck {
            valid: false,
            reason: "handoff is stale".into(),
        };
    }
    if current_session != Some(&packet.native_session) {
        return HandoffCheck {
            valid: false,
            reason: "native conversation does not match".into(),
        };
    }
    HandoffCheck {
        valid: true,
        reason: "fresh and bound to this native conversation".into(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum ContinuityRecord {
    Memory {
        recorded_at_unix: u64,
        role: String,
        scope_id: String,
        terminal_id: String,
        native_session: Option<crate::overseer::SessionIdentity>,
        text: String,
    },
    Handoff {
        packet: HandoffPacket,
    },
}

/// A stable path for a role scope. The digest keeps workspace names out of
/// filenames and avoids treating the current holder as the scope identity.
pub fn memory_path(root: &Path, role_scope: &str) -> PathBuf {
    let digest = sha2::Sha256::digest(role_scope.as_bytes());
    let suffix = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    root.join(format!("{suffix}.jsonl"))
}

pub fn append_record(path: &Path, record: &ContinuityRecord) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "working memory path has no parent",
        )
    })?;
    fs::create_dir_all(parent)?;
    let parent_info = fs::symlink_metadata(parent)?;
    if !parent_info.is_dir() || parent_info.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "working memory directory is not a real directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    if let Ok(info) = fs::symlink_metadata(path) {
        if !info.is_file() || info.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "working memory path is not a regular file",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
    }
    let bytes = serde_json::to_vec(record)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_data()
}

#[cfg(test)]
pub fn read_records(path: &Path) -> io::Result<Vec<ContinuityRecord>> {
    match fs::read_to_string(path) {
        Ok(contents) => contents
            .lines()
            .map(|line| {
                serde_json::from_str(line)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
            })
            .collect(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

/// What one pass over a memory file found. A damaged line is reported and
/// skipped; every other line is still read.
#[derive(Debug, Default)]
pub struct RecordRead {
    pub records: Vec<ContinuityRecord>,
    pub problems: Vec<String>,
}

const PROBLEM_MAX_CHARS: usize = 120;

fn short_problem(text: String) -> String {
    if text.chars().count() > PROBLEM_MAX_CHARS {
        format!(
            "{}…",
            text.chars().take(PROBLEM_MAX_CHARS).collect::<String>()
        )
    } else {
        text
    }
}

pub fn read_records_lenient(path: &Path) -> RecordRead {
    let mut read = RecordRead::default();
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return read,
        Err(error) => {
            read.problems
                .push(short_problem(format!("cannot read: {error}")));
            return read;
        }
    };
    for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice(line) {
            Ok(record) => read.records.push(record),
            Err(error) => read.problems.push(short_problem(format!(
                "line {}: {error} (line skipped)",
                index + 1
            ))),
        }
    }
    read
}

/// What Herdr can do for one harness, declared in code with the evidence behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterLifecycle {
    Supported(&'static str),
    Unsupported(&'static str),
}

impl std::fmt::Display for AdapterLifecycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Supported(note) => write!(f, "supported ({note})"),
            Self::Unsupported(reason) => write!(f, "unsupported ({reason})"),
        }
    }
}

/// Role-card lifecycle per harness. Claude and Codex return the card from their
/// SessionStart hook for verified startup, resume, compact and clear events.
pub fn adapter_lifecycle(agent: Option<crate::detect::Agent>) -> AdapterLifecycle {
    use crate::detect::Agent;
    match agent {
        Some(Agent::Claude) => AdapterLifecycle::Supported(
            "SessionStart returns the card on startup, resume, compact and clear; PreCompact is not used",
        ),
        Some(Agent::Codex) => AdapterLifecycle::Supported(
            "SessionStart returns the card on startup, resume, compact and clear",
        ),
        Some(Agent::Pi) => AdapterLifecycle::Unsupported(
            "session start reports state only, no orientation-context response path",
        ),
        Some(Agent::Cursor) => AdapterLifecycle::Unsupported(
            "turn reports carry session and turn state only, no orientation or context response",
        ),
        Some(_) => AdapterLifecycle::Unsupported("no lifecycle evidence for this harness"),
        None => AdapterLifecycle::Unsupported("harness is not identified"),
    }
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "herdr-role-memory-{label}-{}-{}",
            std::process::id(),
            unix_now()
        ))
    }

    fn entry(text: &str, terminal_id: &str) -> ContinuityRecord {
        ContinuityRecord::Memory {
            recorded_at_unix: unix_now(),
            role: "space_overseer".into(),
            scope_id: "workspace-1".into(),
            terminal_id: terminal_id.into(),
            native_session: None,
            text: text.into(),
        }
    }

    #[test]
    fn role_memory_is_append_only_and_scoped_independently_of_the_holder() {
        let root = temp_root("takeover");
        let path = memory_path(&root.join("memory"), "space:workspace-1");
        let replacement_path = memory_path(&root.join("memory"), "space:workspace-1");
        let other_space = memory_path(&root.join("memory"), "space:workspace-2");
        append_record(&path, &entry("previous holder note", "terminal-old")).unwrap();
        append_record(&replacement_path, &entry("new holder note", "terminal-new")).unwrap();

        assert_eq!(path, replacement_path);
        assert_ne!(path, other_space);
        let entries = read_records(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(
            matches!(&entries[0], ContinuityRecord::Memory { text, .. } if text == "previous holder note")
        );
        assert!(
            matches!(&entries[1], ContinuityRecord::Memory { text, .. } if text == "new holder note")
        );
    }

    #[test]
    fn memory_path_does_not_expose_scope_names() {
        let path = memory_path(Path::new("/state/role-memory"), "space:private-name");
        assert!(!path.display().to_string().contains("private-name"));
        assert!(path
            .extension()
            .is_some_and(|extension| extension == "jsonl"));
    }

    #[test]
    fn handoff_freshness_and_native_conversation_are_checked() {
        let session = (
            "claude".into(),
            "Haiku 5.5".into(),
            crate::agent_resume::AgentSessionRefKind::Id,
            "native-1".into(),
        );
        let packet = HandoffPacket {
            task_id: "R10".into(),
            attempt: "3".into(),
            candidate: "abc".into(),
            tests: vec!["cargo test".into()],
            output_paths: vec!["R10-REPORT.md".into()],
            open_findings: vec!["none".into()],
            cost: "tracked".into(),
            native_session: session.clone(),
            next_owner: "handoff owner".into(),
            recorded_at_unix: 100,
        };
        assert!(check_handoff(&packet, 101, Some(&session)).valid);
        assert_eq!(
            check_handoff(&packet, 100 + HANDOFF_MAX_AGE_SECS + 1, Some(&session)).reason,
            "handoff is stale"
        );
        assert_eq!(
            check_handoff(&packet, 101, None).reason,
            "native conversation does not match"
        );
        let mut incomplete = packet.clone();
        incomplete.tests.clear();
        assert_eq!(
            check_handoff(&incomplete, 101, Some(&session)).reason,
            "tests must be named"
        );
    }

    #[test]
    fn context_transition_never_touches_busy_or_drafted_panes_and_effort_is_in_place() {
        assert_eq!(
            context_transition_plan(true, Some(90), 80, true, false, true, true),
            ContextTransitionPlan::Blocked("pane busy")
        );
        assert_eq!(
            context_transition_plan(true, Some(90), 80, true, true, false, true),
            ContextTransitionPlan::Blocked("input draft present")
        );
        assert_eq!(
            context_transition_plan(true, Some(90), 80, true, true, true, false),
            ContextTransitionPlan::Unsupported(
                "native compact/clear is not accepted for this adapter"
            )
        );
        assert!(!effort_change_requires_clear("low", "high"));
    }

    fn valid_packet() -> (HandoffPacket, crate::overseer::SessionIdentity) {
        let session: crate::overseer::SessionIdentity = (
            "claude".into(),
            "Haiku 5.5".into(),
            crate::agent_resume::AgentSessionRefKind::Id,
            "native-1".into(),
        );
        let packet = HandoffPacket {
            task_id: "R10".into(),
            attempt: "3".into(),
            candidate: "abc".into(),
            tests: vec!["cargo test".into()],
            output_paths: vec!["R10-REPORT.md".into()],
            open_findings: vec!["none".into()],
            cost: "tracked".into(),
            native_session: session.clone(),
            next_owner: "handoff owner".into(),
            recorded_at_unix: 100,
        };
        (packet, session)
    }

    #[test]
    fn each_missing_handoff_field_is_refused_with_its_own_reason() {
        type Break = fn(&mut HandoffPacket);
        let cases: [(&str, Break, &str); 9] = [
            ("task id", |p| p.task_id.clear(), "missing task id"),
            ("attempt", |p| p.attempt = "  ".into(), "missing attempt"),
            ("candidate", |p| p.candidate.clear(), "missing candidate"),
            ("cost", |p| p.cost.clear(), "missing cost"),
            ("next owner", |p| p.next_owner.clear(), "missing next owner"),
            ("tests", |p| p.tests.clear(), "tests must be named"),
            (
                "blank test",
                |p| p.tests = vec![" ".into()],
                "tests must be named",
            ),
            (
                "output paths",
                |p| p.output_paths.clear(),
                "output paths must be named",
            ),
            (
                "open findings",
                |p| p.open_findings.clear(),
                "open findings must be named (write none when there are none)",
            ),
        ];
        let (good, session) = valid_packet();
        assert!(check_handoff(&good, 101, Some(&session)).valid);
        for (label, break_it, reason) in cases {
            let mut packet = good.clone();
            break_it(&mut packet);
            let check = check_handoff(&packet, 101, Some(&session));
            assert!(!check.valid, "{label}");
            assert_eq!(check.reason, reason, "{label}");
        }
        // A blank findings entry is not an explicit "none".
        let mut blank = good;
        blank.open_findings = vec!["".into()];
        assert!(!check_handoff(&blank, 101, Some(&session)).valid);
    }

    #[test]
    fn a_handoff_dated_in_the_future_is_refused() {
        let (packet, session) = valid_packet();
        assert_eq!(
            check_handoff(&packet, 99, Some(&session)).reason,
            "handoff is stale"
        );
    }

    #[cfg(unix)]
    #[test]
    fn memory_directory_and_file_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("modes");
        let path = memory_path(&root.join("memory"), "space:modes");
        append_record(&path, &entry("first", "terminal-a")).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(&path), 0o600);
        // Loosened modes are tightened again on the next append.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
        append_record(&path, &entry("second", "terminal-b")).unwrap();
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(&path), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_memory_directory_or_file_is_refused() {
        use std::os::unix::fs::symlink;
        let root = temp_root("links");
        fs::create_dir_all(&root).unwrap();
        let real_dir = root.join("real");
        fs::create_dir_all(&real_dir).unwrap();
        let linked_dir = root.join("linked-dir");
        symlink(&real_dir, &linked_dir).unwrap();
        let through_dir = linked_dir.join("memory.jsonl");
        let error = append_record(&through_dir, &entry("note", "terminal-a")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(!real_dir.join("memory.jsonl").exists());

        let target = root.join("elsewhere.jsonl");
        fs::write(&target, b"").unwrap();
        let folder = root.join("folder");
        fs::create_dir_all(&folder).unwrap();
        let linked_file = folder.join("memory.jsonl");
        symlink(&target, &linked_file).unwrap();
        let error = append_record(&linked_file, &entry("note", "terminal-a")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read(&target).unwrap(), b"");
    }

    #[test]
    fn a_corrupt_line_is_reported_and_only_that_line_is_skipped() {
        let root = temp_root("corrupt");
        let path = memory_path(&root.join("memory"), "space:corrupt");
        append_record(&path, &entry("before", "terminal-a")).unwrap();
        {
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(b"{not json\n\xff\xfe\n\n").unwrap();
        }
        append_record(&path, &entry("after", "terminal-b")).unwrap();

        assert!(
            read_records(&path).is_err(),
            "the strict reader still fails"
        );
        let read = read_records_lenient(&path);
        assert_eq!(read.records.len(), 2);
        assert_eq!(read.problems.len(), 2, "{:?}", read.problems);
        assert!(
            read.problems[0].starts_with("line 2:"),
            "{:?}",
            read.problems
        );
        assert!(
            read.problems[1].starts_with("line 3:"),
            "{:?}",
            read.problems
        );
        assert!(
            matches!(&read.records[1], ContinuityRecord::Memory { text, .. } if text == "after")
        );
    }

    #[test]
    fn an_unreadable_memory_path_is_a_problem_not_an_empty_memory() {
        let root = temp_root("unreadable");
        let path = root.join("memory.jsonl");
        fs::create_dir_all(&path).unwrap();
        let read = read_records_lenient(&path);
        assert!(read.records.is_empty());
        assert_eq!(read.problems.len(), 1);
        assert!(
            read.problems[0].starts_with("cannot read:"),
            "{:?}",
            read.problems
        );
        assert!(read_records_lenient(&root.join("absent.jsonl"))
            .problems
            .is_empty());
    }

    #[test]
    fn adapter_lifecycle_is_declared_per_harness() {
        use crate::detect::Agent;
        for agent in [Agent::Claude, Agent::Codex] {
            assert!(matches!(
                adapter_lifecycle(Some(agent)),
                AdapterLifecycle::Supported(_)
            ));
        }
        for agent in [Agent::Pi, Agent::Cursor, Agent::Gemini] {
            let status = adapter_lifecycle(Some(agent));
            assert!(
                matches!(status, AdapterLifecycle::Unsupported(reason) if !reason.is_empty()),
                "{agent:?}"
            );
        }
        assert!(adapter_lifecycle(Some(Agent::Claude))
            .to_string()
            .contains("PreCompact is not used"));
        assert!(matches!(
            adapter_lifecycle(None),
            AdapterLifecycle::Unsupported(_)
        ));
    }
}

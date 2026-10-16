//! Invalidate throttled Cursor readings after an explicitly saved login.
use super::{
    atomic_json, cursor_fetch_file, read_json, unknown, Provider, UsageConfig, UsageProblem,
    UsageSnapshot,
};
use std::{
    io,
    sync::atomic::{AtomicU64, Ordering},
};

pub(super) fn revision(config: &UsageConfig) -> Option<String> {
    read_json(&config.snapshot_file.with_extension("cursor-login-revision")).ok()
}

pub(super) fn clear_auth_problem(snapshot: &mut UsageSnapshot) {
    if let Some(cursor) = snapshot
        .providers
        .iter_mut()
        .find(|p| p.provider == Provider::Cursor)
    {
        if matches!(
            cursor.problem,
            Some(UsageProblem::LoginExpired | UsageProblem::NoLogin)
        ) {
            *cursor = unknown(Provider::Cursor, UsageProblem::NoData);
        }
    } else {
        snapshot
            .providers
            .push(unknown(Provider::Cursor, UsageProblem::NoData));
    }
}

/// Reset the disk throttle and publish a revision so already-running workers
/// discard their in-memory throttle too. The hourly import lease is untouched.
pub(crate) fn saved(config: &UsageConfig) -> io::Result<()> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let _lock = crate::platform::usage::lock_statusline(&config.snapshot_file)?;
    let mut snapshot = match read_json::<UsageSnapshot>(&config.snapshot_file) {
        Ok(snapshot) => snapshot,
        Err(error) if error.kind() == io::ErrorKind::NotFound => UsageSnapshot {
            version: 1,
            taken_at: super::now(),
            providers: Vec::new(),
        },
        Err(error) => return Err(error),
    };
    clear_auth_problem(&mut snapshot);
    atomic_json(&config.snapshot_file, &snapshot)?;
    // None is an explicit "due now" value, rather than an old timestamp or a deletion.
    atomic_json(&cursor_fetch_file(config), &Option::<u64>::None)?;
    let revision = format!(
        "{}-{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    atomic_json(
        &config.snapshot_file.with_extension("cursor-login-revision"),
        &revision,
    )
}

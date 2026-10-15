use super::{read_json, unknown, window, Provider, ProviderUsage, UsageProblem, WindowKind};
use serde_json::Value;
use std::path::Path;
pub fn read(path: &Path, now: u64) -> ProviderUsage {
    match read_json::<Value>(path) {
        Ok(v) => parse(&v, now),
        Err(e) => unknown(
            Provider::Claude,
            if e.kind() == std::io::ErrorKind::InvalidData {
                UsageProblem::FormatChanged
            } else {
                UsageProblem::NoData
            },
        ),
    }
}
pub(super) fn parse(v: &Value, now: u64) -> ProviderUsage {
    let mut result = unknown(Provider::Claude, UsageProblem::NoData);
    if !v.is_object() {
        result.problem = Some(UsageProblem::FormatChanged);
        return result;
    }
    result.seen_at = v.get("seen_at").and_then(Value::as_u64);
    if result.seen_at.is_none() {
        result.problem = Some(UsageProblem::FormatChanged);
        return result;
    }
    let windows = [
        window(
            v.get("five_hour"),
            WindowKind::FiveHour,
            "used_percentage",
            now,
        ),
        window(
            v.get("seven_day"),
            WindowKind::Weekly,
            "used_percentage",
            now,
        ),
    ];
    match windows.into_iter().collect::<Result<Vec<_>, _>>() {
        Ok(w) => {
            result.problem = if w.iter().all(|w| w.left_pct.is_none()) {
                Some(UsageProblem::NoData)
            } else {
                None
            };
            result.windows = w;
        }
        Err(()) => result.problem = Some(UsageProblem::FormatChanged),
    }
    result
}

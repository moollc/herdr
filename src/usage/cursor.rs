use super::{percent, timestamp, unknown, Provider, ProviderUsage, UsageProblem};
use serde_json::Value;
use std::time::Duration;
pub(super) fn parse(value: &Value, now: u64) -> ProviderUsage {
    let mut result = unknown(Provider::Cursor, UsageProblem::FormatChanged);
    if value.get("error").and_then(Value::as_str) == Some("not_authenticated") {
        result.problem = Some(UsageProblem::LoginExpired);
        return result;
    }
    let Some(reset) = value
        .get("billingCycleEnd")
        .and_then(Value::as_str)
        .and_then(timestamp)
    else {
        return result;
    };
    let Some(plan) = value
        .pointer("/individualUsage/plan")
        .and_then(Value::as_object)
    else {
        return result;
    };
    for key in ["autoPercentUsed", "apiPercentUsed", "totalPercentUsed"] {
        if plan.get(key).is_some_and(|v| percent(v).is_none()) {
            return result;
        }
    }
    let auto = plan.get("autoPercentUsed").and_then(percent);
    let api = plan.get("apiPercentUsed").and_then(percent);
    let overall = plan.get("totalPercentUsed").and_then(percent);
    let unlimited = value.get("isUnlimited").and_then(Value::as_bool) == Some(true);
    let tighter = match (auto, api) {
        (Some(a), Some(b)) if a <= b => "auto",
        (Some(_), Some(_)) => "named",
        (Some(_), None) => "auto",
        (None, Some(_)) => "named",
        _ => "unknown",
    };
    let show = |v: Option<u8>| v.map_or("?".into(), |n| format!("{n}%"));
    let on_demand = value
        .pointer("/individualUsage/onDemand")
        .filter(|v| v.get("enabled").and_then(Value::as_bool) == Some(true));
    let note = format!(
        "overall {} left, auto {} left, named {} left, tighter {tighter}{}, breakdown={}{}",
        show(overall),
        show(auto),
        show(api),
        if unlimited { ", unlimited" } else { "" },
        plan.get("breakdown").unwrap_or(&Value::Null),
        on_demand.map_or(String::new(), |v| format!(", on_demand={v}"))
    );
    let passed = reset <= now && !unlimited;
    result.seen_at = Some(now);
    result.windows = vec![
        super::UsageWindow {
            kind: super::WindowKind::Auto,
            left_pct: if unlimited { None } else { auto },
            resets_at: Some(reset),
            note: Some(note),
            reset: passed && auto.is_some(),
        },
        super::UsageWindow {
            kind: super::WindowKind::Named,
            left_pct: if unlimited { None } else { api },
            resets_at: Some(reset),
            note: None,
            reset: passed && api.is_some(),
        },
    ];
    result.problem = if auto.is_some() || api.is_some() || unlimited {
        None
    } else {
        Some(UsageProblem::FormatChanged)
    };
    result
}
pub(super) fn fetch(service: &str, now: u64) -> ProviderUsage {
    // Read the cookie only when refresh has decided that a fetch is due.
    // Never include command output, headers or request errors in a log.
    let Some(cookie) = crate::platform::usage::cursor_cookie(service) else {
        return unknown(Provider::Cursor, UsageProblem::NoLogin);
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into();
    let response = agent
        .get("https://cursor.com/api/usage-summary")
        .header("Cookie", &cookie)
        .call();
    let Ok(mut response) = response else {
        return unknown(Provider::Cursor, UsageProblem::Offline);
    };
    let status = response.status().as_u16();
    let location = response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    if status != 200 {
        return http_result(status, &location, "", now);
    }
    match response
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_string()
    {
        Ok(body) => http_result(status, &location, &body, now),
        Err(_) => unknown(Provider::Cursor, UsageProblem::FormatChanged),
    }
}
pub(super) fn http_result(status: u16, location: &str, body: &str, now: u64) -> ProviderUsage {
    if status == 401 || (status == 307 && location.starts_with("https://api.workos.com/")) {
        return unknown(Provider::Cursor, UsageProblem::LoginExpired);
    }
    if status == 403 || (300..400).contains(&status) {
        return unknown(Provider::Cursor, UsageProblem::FormatChanged);
    }
    if status != 200 {
        return unknown(Provider::Cursor, UsageProblem::Offline);
    }
    match serde_json::from_str(body) {
        Ok(value) => parse(&value, now),
        Err(_) => unknown(Provider::Cursor, UsageProblem::FormatChanged),
    }
}

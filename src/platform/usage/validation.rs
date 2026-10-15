//! Validate an explicitly supplied login without exposing it in diagnostics.
use std::time::{Duration, Instant};
pub(super) fn check_usage(secret: &str, deadline: Instant) -> i32 {
    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
        return 1;
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(remaining.min(Duration::from_secs(5))))
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into();
    let Ok(mut response) = agent
        .get("https://cursor.com/api/usage-summary")
        .header("Cookie", secret)
        .call()
    else {
        return 1;
    };
    let status = response.status().as_u16();
    let location = response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if status != 200 {
        return validation(status, location, None);
    }
    let body = response
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_json::<serde_json::Value>()
        .ok();
    validation(status, "", body.as_ref())
}
pub(super) fn validation(status: u16, location: &str, body: Option<&serde_json::Value>) -> i32 {
    if status == 401
        || ((300..400).contains(&status) && location.starts_with("https://api.workos.com/"))
    {
        return 4;
    }
    if status == 200
        && body
            .and_then(|v| v.get("billingCycleEnd"))
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty())
    {
        0
    } else {
        1
    }
}

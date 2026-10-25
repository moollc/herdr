//! OpenRouter allowance readings: the daily free request count and the paid credit balance.
//!
//! Two independent readings with separate ages. The free count comes from
//! `GET /api/v1/key` (`data.free_model_daily_requests`, for the current UTC day) with
//! the inference key. The credit balance comes from `GET /api/v1/credits`
//! (`data.total_credits` and `data.total_usage`) and needs a Management key; it
//! answers 403 without one. The key's own `limit_remaining` is not account credit and
//! is never read. Money is exact: a JSON number's decimal text becomes integer
//! micro-dollars, so no float arithmetic touches a balance.
//!
//! Live network access is confined to `fetch_live`. Everything else is pure, so the
//! parsers, windows and text are tested with fixtures.
use super::{unknown, Provider, ProviderUsage, UsageConfig, UsageProblem, UsageWindow, WindowKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fs, io::Read, path::Path, time::Duration};

pub(crate) const FREE_URL: &str = "https://openrouter.ai/api/v1/key";
pub(crate) const CREDITS_URL: &str = "https://openrouter.ai/api/v1/credits";
/// The app worker may poll this often. A live fetch is never started sooner.
pub(crate) const POLL_MIN_INTERVAL_SECS: u64 = 60;
/// A reading older than this is drawn dimmed. Polls run every minute, so this is several missed polls.
pub(crate) const STALE_AFTER_SECS: u64 = 5 * 60;
const TIMEOUT_SECS: u64 = 5;
const BODY_LIMIT: u64 = 64 * 1024;
const KEY_FILE_LIMIT: u64 = 4 * 1024;
const KEY_MAX_LEN: usize = 512;
const MICROS_PER_USD: i64 = 1_000_000;
const DAY_SECS: u64 = 86_400;

/// The free daily count as the provider reported it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreeDaily {
    pub used: u64,
    pub limit: u64,
    pub remaining: u64,
    pub seen_at: u64,
}

/// The credit totals in micro-dollars. The balance is total minus usage and may be negative.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreditBalance {
    pub total_micros: i64,
    pub usage_micros: i64,
    pub seen_at: u64,
}

impl CreditBalance {
    pub fn balance_micros(&self) -> i64 {
        self.total_micros.saturating_sub(self.usage_micros)
    }
}

/// What the provider row keeps. Each reading has its own age and problem. No key is stored.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenRouterDetail {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free: Option<FreeDaily>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_problem: Option<UsageProblem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credit: Option<CreditBalance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credit_problem: Option<UsageProblem>,
}

/// One poll's answer for each reading, before it is merged with the last good ones.
#[derive(Clone, Debug, PartialEq)]
pub struct Fetched {
    pub free: Result<FreeDaily, UsageProblem>,
    pub credit: Result<CreditBalance, UsageProblem>,
}

impl Fetched {
    /// Nothing configured: both readings say so, and no request is made.
    pub fn none() -> Self {
        Self {
            free: Err(UsageProblem::NoLogin),
            credit: Err(UsageProblem::NoLogin),
        }
    }
}

/// Exact decimal text to micro-dollars. Up to six decimals; further digits truncate toward
/// zero. Exponents, stray signs, empty or non-digit parts are refused.
pub(crate) fn decimal_micros(text: &str) -> Option<i64> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    let plain = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty() || whole.len() > 12 || !plain(whole) || !plain(fraction) {
        return None;
    }
    let mut padded: String = fraction.chars().take(6).collect();
    while padded.len() < 6 {
        padded.push('0');
    }
    let whole: i64 = whole.parse().ok()?;
    let fraction: i64 = padded.parse().ok()?;
    let magnitude = whole.checked_mul(MICROS_PER_USD)?.checked_add(fraction)?;
    Some(if negative { -magnitude } else { magnitude })
}

/// A JSON number, read through its decimal text. Strings and absent fields are refused.
fn json_micros(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => decimal_micros(&number.to_string()),
        _ => None,
    }
}

/// `9.90$`: numeric before the symbol, two decimals, truncated toward zero. `-0.10$` when owed.
pub(crate) fn format_usd(micros: i64) -> String {
    let cents = micros / 10_000;
    let sign = if cents < 0 { "-" } else { "" };
    let magnitude = cents.unsigned_abs();
    format!("{sign}{}.{:02}$", magnitude / 100, magnitude % 100)
}

/// The share of the reported total still left, floored and clamped to 0..=100. Owed balances
/// are 0. A total that is zero or below gives no scale, so the bar stays unknown.
pub(crate) fn credit_left_pct(balance: i64, total: i64) -> Option<u8> {
    if total <= 0 {
        return None;
    }
    if balance <= 0 {
        return Some(0);
    }
    let pct = i128::from(balance) * 100 / i128::from(total);
    Some(pct.min(100) as u8)
}

fn field_u64(object: &Value, name: &str) -> Result<u64, UsageProblem> {
    match object.get(name) {
        None => Err(UsageProblem::NoData),
        Some(value) => value.as_u64().ok_or(UsageProblem::FormatChanged),
    }
}

fn field_micros(object: &Value, name: &str) -> Result<i64, UsageProblem> {
    match object.get(name) {
        None => Err(UsageProblem::NoData),
        Some(value) => json_micros(value).ok_or(UsageProblem::FormatChanged),
    }
}

/// `GET /api/v1/key`. A missing `free_model_daily_requests` is no data. Fields that are not
/// whole numbers, a zero limit, or remaining above limit are a format change.
pub(crate) fn parse_free(body: &Value, now: u64) -> Result<FreeDaily, UsageProblem> {
    let data = body.get("data").ok_or(UsageProblem::NoData)?;
    let daily = data
        .get("free_model_daily_requests")
        .ok_or(UsageProblem::NoData)?;
    let used = field_u64(daily, "used")?;
    let limit = field_u64(daily, "limit")?;
    let remaining = field_u64(daily, "remaining")?;
    if limit == 0 || remaining > limit {
        return Err(UsageProblem::FormatChanged);
    }
    Ok(FreeDaily {
        used,
        limit,
        remaining,
        seen_at: now,
    })
}

/// `GET /api/v1/credits`. A zero total is a real reading: `0.00$`, with no scale for the bar.
/// A negative total or usage is malformed. A negative balance (total below usage) is kept, not clamped.
pub(crate) fn parse_credit(body: &Value, now: u64) -> Result<CreditBalance, UsageProblem> {
    let data = body.get("data").ok_or(UsageProblem::NoData)?;
    let total = field_micros(data, "total_credits")?;
    let usage = field_micros(data, "total_usage")?;
    if total < 0 || usage < 0 {
        return Err(UsageProblem::FormatChanged);
    }
    Ok(CreditBalance {
        total_micros: total,
        usage_micros: usage,
        seen_at: now,
    })
}

/// Free counts for the current UTC day only. An older reading has no current count.
pub(crate) fn free_today(detail: &OpenRouterDetail, now: u64) -> Option<&FreeDaily> {
    detail
        .free
        .as_ref()
        .filter(|free| free.seen_at <= now && utc_day(free.seen_at) == utc_day(now))
}

fn utc_day(at: u64) -> u64 {
    at / DAY_SECS
}

/// The next UTC midnight after `now`, where the free allowance renews.
pub(crate) fn next_utc_midnight(now: u64) -> u64 {
    (utc_day(now) + 1) * DAY_SECS
}

fn percent_floor(part: u64, whole: u64) -> u8 {
    (u128::from(part) * 100 / u128::from(whole)).min(100) as u8
}

fn is_stale(seen_at: u64, now: u64) -> bool {
    now.saturating_sub(seen_at) > STALE_AFTER_SECS
}

/// The two windows the provider row draws. The free window has a reset only while its
/// reading is from today (UTC). Otherwise it is `?` until a new reading arrives: no
/// allowance is assumed after midnight. The credit window has no reset.
pub(crate) fn windows(detail: &OpenRouterDetail, now: u64) -> Vec<UsageWindow> {
    let free = free_today(detail, now);
    let daily_left = free.map(|free| {
        let pct = percent_floor(free.remaining, free.limit);
        if free.remaining > 0 {
            pct.max(1)
        } else {
            0
        }
    });
    let credit_left = detail
        .credit
        .as_ref()
        .and_then(|credit| credit_left_pct(credit.balance_micros(), credit.total_micros));
    vec![
        UsageWindow {
            kind: WindowKind::Daily,
            left_pct: daily_left,
            resets_at: free.map(|_| next_utc_midnight(now)),
            note: None,
            reset: false,
        },
        UsageWindow {
            kind: WindowKind::Credit,
            left_pct: credit_left,
            resets_at: None,
            note: None,
            reset: false,
        },
    ]
}

/// The number drawn beside a window, and whether that reading is stale. `?` when unknown.
pub(crate) fn number_text(detail: &OpenRouterDetail, kind: WindowKind, now: u64) -> (String, bool) {
    match kind {
        WindowKind::Credit => match detail.credit.as_ref() {
            Some(credit) => (
                format_usd(credit.balance_micros()),
                is_stale(credit.seen_at, now),
            ),
            None => ("?".into(), false),
        },
        _ => match free_today(detail, now) {
            Some(free) => {
                let pct = percent_floor(free.remaining, free.limit);
                let text = if free.remaining > 0 && pct == 0 {
                    "<1%".to_string()
                } else {
                    format!("{pct}%")
                };
                (text, is_stale(free.seen_at, now))
            }
            None => ("?".into(), false),
        },
    }
}

/// The plain `herdr usage` line for one OpenRouter reading.
pub(crate) fn cli_line(detail: &OpenRouterDetail, kind: WindowKind, now: u64) -> String {
    match kind {
        WindowKind::Credit => match detail.credit.as_ref() {
            Some(credit) => format!(
                "{} balance, total {}, used {}",
                format_usd(credit.balance_micros()),
                format_usd(credit.total_micros),
                format_usd(credit.usage_micros)
            ),
            None => format!(
                "credit unavailable: {}",
                detail
                    .credit_problem
                    .as_ref()
                    .map_or("no reading".to_string(), |p| format!("{p:?}"))
            ),
        },
        _ => match free_today(detail, now) {
            Some(free) => format!(
                "{} of {} free requests remain, {} used, {}",
                free.remaining,
                free.limit,
                free.used,
                number_text(detail, kind, now).0
            ),
            None => format!(
                "free count unavailable: {}",
                detail
                    .free_problem
                    .as_ref()
                    .map_or("awaiting today's reading".to_string(), |p| format!("{p:?}"))
            ),
        },
    }
}

/// A provider row with no reading yet: the windows say `?`.
pub(crate) fn placeholder() -> ProviderUsage {
    unknown(Provider::OpenRouter, UsageProblem::NoLogin)
}

/// Maps a non-200 answer to a problem. `None` for 200, which is read further.
pub(crate) fn problem_for_status(status: u16) -> Option<UsageProblem> {
    match status {
        200 => None,
        401 => Some(UsageProblem::LoginExpired),
        403 => Some(UsageProblem::PermissionDenied),
        429 => Some(UsageProblem::RateLimited),
        _ => Some(UsageProblem::Offline),
    }
}

/// Validates a token. ASCII graphic only, no spaces or control characters, bounded length.
fn valid_token(token: &str) -> Option<String> {
    let ok = !token.is_empty()
        && token.len() <= KEY_MAX_LEN
        && token.bytes().all(|byte| byte.is_ascii_graphic());
    ok.then(|| token.to_string())
}

/// The name of the one assignment a key file may hold.
const ASSIGNMENT_NAME: &str = "OPENROUTER_API_KEY";

/// The key in a key file's text. Exactly one line, optionally `OPENROUTER_API_KEY=value`
/// with the value in matching simple quotes, or one bare token. Nothing is executed or
/// expanded. Extra assignments, commands, extra lines, and any character outside letters,
/// digits, `-`, `_` and `.` are refused.
pub(crate) fn key_from_file_text(text: &str) -> Option<String> {
    let line = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text);
    if line.contains(['\n', '\r']) {
        return None;
    }
    let value = match line
        .strip_prefix(ASSIGNMENT_NAME)
        .and_then(|rest| rest.strip_prefix('='))
    {
        Some(rest) => unquote(rest),
        None if line == ASSIGNMENT_NAME => return None,
        None => line,
    };
    let plain = !value.is_empty()
        && value.len() <= KEY_MAX_LEN
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    plain.then(|| value.to_string())
}

/// The value inside one pair of matching simple quotes, or the value as it is.
fn unquote(rest: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = rest.strip_prefix(quote).and_then(|r| r.strip_suffix(quote)) {
            return inner;
        }
    }
    rest
}

/// A key from an owner-only key file, or from a named environment variable.
/// The file wins when both are set. The value is never logged, echoed or stored.
pub(crate) fn load_key(file: Option<&Path>, env_name: Option<&str>) -> Option<String> {
    match (file, env_name.map(str::trim)) {
        (Some(path), _) => key_from_file_text(&read_private_file(path)?),
        (None, Some(name)) if !name.is_empty() => valid_token(std::env::var(name).ok()?.trim()),
        _ => None,
    }
}

/// The file's text, read only when the opened file is the current user's, owner-only, and
/// within the size limit. The check runs on the open file, so a symlink is judged by its target.
fn read_private_file(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    if !crate::platform::usage::key_file_is_private(&file) {
        tracing::warn!("openrouter key file is missing or not owner-only; not read");
        return None;
    }
    let mut data = Vec::new();
    file.take(KEY_FILE_LIMIT + 1).read_to_end(&mut data).ok()?;
    if data.len() as u64 > KEY_FILE_LIMIT {
        return None;
    }
    String::from_utf8(data).ok()
}

/// One bounded GET with the inference or management key. No retry. Errors carry only a
/// problem kind, never the body, the URL, or the key.
fn get_json(url: &str, key: &str) -> Result<Value, UsageProblem> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(TIMEOUT_SECS)))
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into();
    let authorization = format!("Bearer {key}");
    let mut response = agent
        .get(url)
        .header("Authorization", &authorization)
        .call()
        .map_err(|_| UsageProblem::Offline)?;
    if let Some(problem) = problem_for_status(response.status().as_u16()) {
        return Err(problem);
    }
    let body = response
        .body_mut()
        .with_config()
        .limit(BODY_LIMIT)
        .read_to_string()
        .map_err(|_| UsageProblem::FormatChanged)?;
    serde_json::from_str(&body).map_err(|_| UsageProblem::FormatChanged)
}

/// Reads both endpoints with the configured keys. Nothing is requested for a reading whose key
/// is not configured. Called only from the app worker or the CLI, never from render.
pub(crate) fn fetch_live(config: &UsageConfig, now: u64) -> Fetched {
    let free = match load_key(
        config.openrouter_key_file.as_deref(),
        config.openrouter_key_env.as_deref(),
    ) {
        None => Err(UsageProblem::NoLogin),
        Some(key) => get_json(FREE_URL, &key).and_then(|body| parse_free(&body, now)),
    };
    let credit = match load_key(config.openrouter_management_key_file.as_deref(), None) {
        None => Err(UsageProblem::NoLogin),
        Some(key) => get_json(CREDITS_URL, &key).and_then(|body| parse_credit(&body, now)),
    };
    Fetched { free, credit }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: u64 = 1_800_000_000; // a fixed instant, mid UTC day

    fn free(remaining: u64, limit: u64, seen_at: u64) -> FreeDaily {
        FreeDaily {
            used: limit - remaining,
            limit,
            remaining,
            seen_at,
        }
    }

    fn credit(total: i64, usage: i64, seen_at: u64) -> CreditBalance {
        CreditBalance {
            total_micros: total,
            usage_micros: usage,
            seen_at,
        }
    }

    #[test]
    fn decimal_text_is_exact_and_refuses_what_is_not_plain_decimal() {
        assert_eq!(decimal_micros("9.9"), Some(9_900_000));
        assert_eq!(decimal_micros("10"), Some(10_000_000));
        assert_eq!(decimal_micros("0.1"), Some(100_000));
        assert_eq!(decimal_micros("-0.1"), Some(-100_000));
        assert_eq!(decimal_micros("1.2345678"), Some(1_234_567), "truncates");
        assert_eq!(decimal_micros("-1.2345678"), Some(-1_234_567));
        assert_eq!(decimal_micros("1e3"), None);
        assert_eq!(decimal_micros("+1"), None);
        assert_eq!(decimal_micros(""), None);
        assert_eq!(decimal_micros("-"), None);
        assert_eq!(decimal_micros("1.2.3"), None);
        assert_eq!(decimal_micros("abc"), None);
        assert_eq!(decimal_micros("9999999999999"), None, "bounded");
    }

    #[test]
    fn ninety_nine_cents_add_up_without_float_error() {
        // 0.1 + 0.2 is not 0.3 in binary floats. Micro-dollars stay exact.
        let total = decimal_micros("0.1").unwrap() + decimal_micros("0.2").unwrap();
        assert_eq!(total, decimal_micros("0.3").unwrap());
        assert_eq!(format_usd(total), "0.30$");
    }

    #[test]
    fn usd_is_numeric_before_the_symbol_with_two_decimals() {
        assert_eq!(format_usd(9_900_000), "9.90$");
        assert_eq!(format_usd(0), "0.00$");
        assert_eq!(format_usd(-100_000), "-0.10$");
        assert_eq!(format_usd(-1_010_000), "-1.01$");
        assert_eq!(
            format_usd(-1_000),
            "0.00$",
            "sub-cent owed is not shown as -0.00"
        );
        assert_eq!(format_usd(1_234_567), "1.23$");
    }

    #[test]
    fn free_count_comes_from_the_api_field_and_missing_is_no_data() {
        let body = json!({"data": {"free_model_daily_requests": {"used": 130, "limit": 1000, "remaining": 870}}});
        assert_eq!(parse_free(&body, NOW), Ok(free(870, 1000, NOW)));
        // A usage-looking field elsewhere is not the free count.
        let elsewhere = json!({"data": {"usage": 130, "limit_remaining": 5}});
        assert_eq!(parse_free(&elsewhere, NOW), Err(UsageProblem::NoData));
        assert_eq!(
            parse_free(&json!({"data": {}}), NOW),
            Err(UsageProblem::NoData)
        );
    }

    #[test]
    fn malformed_free_fields_are_a_format_change_not_a_guess() {
        let fractional = json!({"data": {"free_model_daily_requests": {"used": 1.5, "limit": 1000, "remaining": 870}}});
        assert_eq!(
            parse_free(&fractional, NOW),
            Err(UsageProblem::FormatChanged)
        );
        let zero_limit =
            json!({"data": {"free_model_daily_requests": {"used": 0, "limit": 0, "remaining": 0}}});
        assert_eq!(
            parse_free(&zero_limit, NOW),
            Err(UsageProblem::FormatChanged)
        );
        let over = json!({"data": {"free_model_daily_requests": {"used": 0, "limit": 10, "remaining": 11}}});
        assert_eq!(parse_free(&over, NOW), Err(UsageProblem::FormatChanged));
        let missing_used =
            json!({"data": {"free_model_daily_requests": {"limit": 10, "remaining": 1}}});
        assert_eq!(parse_free(&missing_used, NOW), Err(UsageProblem::NoData));
    }

    #[test]
    fn zero_free_remaining_is_real_exhaustion_and_one_request_is_not_zero_percent() {
        let empty = windows(&detail_free(free(0, 1000, NOW)), NOW);
        assert_eq!(empty[0].left_pct, Some(0));
        assert_eq!(
            number_text(&detail_free(free(0, 1000, NOW)), WindowKind::Daily, NOW).0,
            "0%"
        );
        let one = detail_free(free(1, 1000, NOW));
        assert_eq!(number_text(&one, WindowKind::Daily, NOW).0, "<1%");
        assert_eq!(
            windows(&one, NOW)[0].left_pct,
            Some(1),
            "one request still lights a square"
        );
        assert_eq!(
            number_text(&detail_free(free(870, 1000, NOW)), WindowKind::Daily, NOW).0,
            "87%"
        );
    }

    fn detail_free(f: FreeDaily) -> OpenRouterDetail {
        OpenRouterDetail {
            free: Some(f),
            ..Default::default()
        }
    }

    #[test]
    fn exhausted_free_with_positive_credit_keeps_both_readings_separate() {
        let detail = OpenRouterDetail {
            free: Some(free(0, 1000, NOW)),
            credit: Some(credit(10_000_000, 100_000, NOW)),
            ..Default::default()
        };
        let windows = windows(&detail, NOW);
        assert_eq!(windows[0].left_pct, Some(0), "free is empty");
        assert_eq!(
            windows[1].left_pct,
            Some(99),
            "credit still has 9.90 of 10.00"
        );
        assert_eq!(number_text(&detail, WindowKind::Credit, NOW).0, "9.90$");
    }

    #[test]
    fn credit_is_total_minus_usage_and_negative_balances_stay_visible() {
        let positive = credit(10_000_000, 100_000, NOW);
        assert_eq!(positive.balance_micros(), 9_900_000);
        let owed = credit(1_000_000, 1_010_000, NOW);
        assert_eq!(owed.balance_micros(), -10_000);
        let detail = OpenRouterDetail {
            credit: Some(owed),
            ..Default::default()
        };
        assert_eq!(number_text(&detail, WindowKind::Credit, NOW).0, "-0.01$");
        assert_eq!(
            windows(&detail, NOW)[1].left_pct,
            Some(0),
            "owed is empty, never negative"
        );
    }

    #[test]
    fn credit_totals_must_be_real_numbers_with_a_positive_scale() {
        let body = json!({"data": {"total_credits": 10.0, "total_usage": 0.1}});
        let parsed = parse_credit(&body, NOW).unwrap();
        assert_eq!(parsed.balance_micros(), 9_900_000);
        let zero = parse_credit(
            &json!({"data": {"total_credits": 0, "total_usage": 0}}),
            NOW,
        )
        .expect("a zero total is a known reading");
        assert_eq!(zero.balance_micros(), 0);
        assert_eq!(
            format_usd(zero.balance_micros()),
            "0.00$",
            "known zero, not an error"
        );
        assert_eq!(credit_left_pct(0, 0), None, "no scale for the bar");
        assert_eq!(
            parse_credit(
                &json!({"data": {"total_credits": -1, "total_usage": 0}}),
                NOW
            ),
            Err(UsageProblem::FormatChanged)
        );
        assert_eq!(
            parse_credit(
                &json!({"data": {"total_credits": "10", "total_usage": 0}}),
                NOW
            ),
            Err(UsageProblem::FormatChanged),
            "strings are not money"
        );
        assert_eq!(
            parse_credit(&json!({"data": {"total_credits": 10}}), NOW),
            Err(UsageProblem::NoData),
            "missing usage is no data"
        );
    }

    #[test]
    fn credit_left_is_floored_and_bounded() {
        assert_eq!(credit_left_pct(9_900_000, 10_000_000), Some(99));
        assert_eq!(credit_left_pct(10_000_000, 10_000_000), Some(100));
        assert_eq!(
            credit_left_pct(20_000_000, 10_000_000),
            Some(100),
            "over-total clamps"
        );
        assert_eq!(credit_left_pct(-1, 10_000_000), Some(0));
        assert_eq!(credit_left_pct(5, 0), None);
    }

    #[test]
    fn free_window_resets_at_utc_midnight_and_waits_for_a_new_reading_after_it() {
        let seen_today = NOW - 60;
        let today = OpenRouterDetail {
            free: Some(free(870, 1000, seen_today)),
            ..Default::default()
        };
        let w = windows(&today, NOW);
        assert_eq!(w[0].resets_at, Some(next_utc_midnight(NOW)));
        // After midnight the old reading is not today's: no reset, no assumed full allowance.
        let after_midnight = next_utc_midnight(NOW) + 5;
        let w = windows(&today, after_midnight);
        assert_eq!(w[0].left_pct, None);
        assert_eq!(w[0].resets_at, None);
        assert_eq!(
            number_text(&today, WindowKind::Daily, after_midnight).0,
            "?"
        );
    }

    #[test]
    fn a_stale_reading_dims_only_its_own_row() {
        let old = NOW - STALE_AFTER_SECS - 1;
        let detail = OpenRouterDetail {
            free: Some(free(870, 1000, old)),
            credit: Some(credit(10_000_000, 100_000, NOW)),
            ..Default::default()
        };
        let (free_text, free_stale) = number_text(&detail, WindowKind::Daily, NOW);
        let (credit_text, credit_stale) = number_text(&detail, WindowKind::Credit, NOW);
        assert_eq!(
            (free_text.as_str(), free_stale),
            ("87%", true),
            "free ages alone"
        );
        assert_eq!(
            (credit_text.as_str(), credit_stale),
            ("9.90$", false),
            "credit is fresh"
        );
    }

    #[test]
    fn status_codes_map_without_echoing_bodies() {
        assert_eq!(problem_for_status(200), None);
        assert_eq!(problem_for_status(401), Some(UsageProblem::LoginExpired));
        assert_eq!(
            problem_for_status(403),
            Some(UsageProblem::PermissionDenied)
        );
        assert_eq!(problem_for_status(429), Some(UsageProblem::RateLimited));
        assert_eq!(problem_for_status(500), Some(UsageProblem::Offline));
        assert_eq!(problem_for_status(402), Some(UsageProblem::Offline));
    }

    #[test]
    fn a_kept_reading_still_shows_its_number_while_it_is_stale() {
        let kept = OpenRouterDetail {
            free: Some(free(870, 1000, NOW - 600)),
            free_problem: Some(UsageProblem::RateLimited),
            ..Default::default()
        };
        let (text, stale) = number_text(&kept, WindowKind::Daily, NOW);
        assert_eq!((text.as_str(), stale), ("87%", true));
    }

    #[cfg(unix)]
    #[test]
    fn keys_come_only_from_owner_files_or_named_environment_variables() {
        let dir =
            std::env::temp_dir().join(format!("herdr-openrouter-keys-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let private = dir.join("private-key");
        fs::write(&private, "sk-or-test-token-value\n").unwrap();
        set_mode(&private, 0o600);
        assert_eq!(
            load_key(Some(&private), None).as_deref(),
            Some("sk-or-test-token-value")
        );
        let shared = dir.join("shared-key");
        fs::write(&shared, "sk-or-test-token-value\n").unwrap();
        set_mode(&shared, 0o644);
        assert_eq!(
            load_key(Some(&shared), None),
            None,
            "group or other readable is refused"
        );
        let spaced = dir.join("spaced-key");
        fs::write(&spaced, "sk-or part two\n").unwrap();
        set_mode(&spaced, 0o600);
        assert_eq!(
            load_key(Some(&spaced), None),
            None,
            "whitespace is not a token"
        );
        let name = format!("HERDR_TEST_OPENROUTER_KEY_{}", std::process::id());
        std::env::set_var(&name, "sk-or-env-token");
        assert_eq!(
            load_key(None, Some(&name)).as_deref(),
            Some("sk-or-env-token")
        );
        assert_eq!(load_key(None, None), None);
        std::env::remove_var(&name);
        assert_eq!(load_key(Some(&dir.join("absent")), None), None);
    }

    #[test]
    fn a_key_file_holds_one_assignment_or_one_bare_token_and_nothing_else() {
        let key = |text: &str| key_from_file_text(text);
        assert_eq!(
            key("OPENROUTER_API_KEY=sk-or-v1-abc123\n").as_deref(),
            Some("sk-or-v1-abc123")
        );
        assert_eq!(
            key("OPENROUTER_API_KEY=\"sk-or-v1-abc123\"\n").as_deref(),
            Some("sk-or-v1-abc123")
        );
        assert_eq!(
            key("OPENROUTER_API_KEY='sk-or-v1-abc123'").as_deref(),
            Some("sk-or-v1-abc123"),
            "no final newline"
        );
        assert_eq!(
            key("sk-or-v1-abc123\n").as_deref(),
            Some("sk-or-v1-abc123"),
            "a bare token still works"
        );
        assert_eq!(
            key("OPENROUTER_API_KEY=\"sk-or-v1-abc123'\n"),
            None,
            "mismatched quotes"
        );
        assert_eq!(
            key("OPENROUTER_API_KEY=sk-a OPENROUTER_API_KEY=sk-b\n"),
            None,
            "second assignment on the line"
        );
        assert_eq!(
            key("OPENROUTER_API_KEY=sk-a\nOPENROUTER_API_KEY=sk-b\n"),
            None,
            "two lines"
        );
        assert_eq!(
            key("OPENROUTER_API_KEY=a\r\nb\n"),
            None,
            "carriage return and extra line"
        );
        assert_eq!(
            key("OPENROUTER_API_KEY=$(cat /etc/passwd)\n"),
            None,
            "no command substitution"
        );
        assert_eq!(key("OPENROUTER_API_KEY=`id`\n"), None, "no backticks");
        assert_eq!(
            key("OPENROUTER_API_KEY=sk-a;rm\n"),
            None,
            "no shell separators"
        );
        assert_eq!(
            key("export OPENROUTER_API_KEY=sk-a\n"),
            None,
            "no export prefix"
        );
        assert_eq!(key("OPENROUTER_API_KEY=\n"), None, "empty value");
        assert_eq!(key("OPENROUTER_API_KEY\n"), None, "name without a value");
        assert_eq!(key("FOO=bar\n"), None, "other assignment names are refused");
    }

    #[test]
    fn a_known_zero_balance_shows_zero_dollars_with_no_scale_for_the_bar() {
        let detail = OpenRouterDetail {
            credit: Some(credit(0, 0, NOW)),
            ..Default::default()
        };
        let (text, stale) = number_text(&detail, WindowKind::Credit, NOW);
        assert_eq!((text.as_str(), stale), ("0.00$", false));
        let left = windows(&detail, NOW)
            .into_iter()
            .find(|w| w.kind == WindowKind::Credit)
            .and_then(|w| w.left_pct);
        assert_eq!(left, None, "a zero total gives the bar no scale");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_an_owner_only_key_file_is_read_and_a_link_to_a_shared_file_is_not() {
        use std::os::unix::fs::symlink;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "herdr-openrouter-link-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let private = dir.join("openrouter.env");
        fs::write(&private, "OPENROUTER_API_KEY=sk-or-v1-linked\n").unwrap();
        set_mode(&private, 0o600);
        let link = dir.join("link.env");
        symlink(&private, &link).unwrap();
        assert_eq!(
            load_key(Some(&link), None).as_deref(),
            Some("sk-or-v1-linked")
        );
        let shared = dir.join("shared.env");
        fs::write(&shared, "OPENROUTER_API_KEY=sk-or-v1-shared\n").unwrap();
        set_mode(&shared, 0o644);
        let shared_link = dir.join("shared-link.env");
        symlink(&shared, &shared_link).unwrap();
        assert_eq!(
            load_key(Some(&shared_link), None),
            None,
            "the target is group or other readable"
        );
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn with_no_keys_configured_nothing_is_requested_and_both_readings_say_no_login() {
        let config = UsageConfig {
            openrouter_key_file: None,
            openrouter_key_env: None,
            openrouter_management_key_file: None,
            ..UsageConfig::default()
        };
        assert_eq!(fetch_live(&config, NOW), Fetched::none());
    }

    #[test]
    fn serialized_openrouter_state_never_contains_a_key() {
        let key = "sk-or-secret-must-not-appear";
        let detail = OpenRouterDetail {
            free: Some(free(870, 1000, NOW)),
            free_problem: Some(UsageProblem::LoginExpired),
            credit: Some(credit(10_000_000, 100_000, NOW)),
            credit_problem: None,
        };
        let provider = ProviderUsage {
            provider: Provider::OpenRouter,
            extra_spend: None,
            openrouter: Some(detail),
            windows: windows(&OpenRouterDetail::default(), NOW),
            seen_at: Some(NOW),
            source: "openrouter api".into(),
            problem: None,
        };
        let text = serde_json::to_string(&provider).unwrap();
        assert!(!text.contains(key));
        assert!(!text.contains("Bearer"));
        assert!(text.contains("\"openrouter\""));
    }

    #[test]
    fn cli_lines_say_what_is_known_and_what_is_not() {
        let detail = OpenRouterDetail {
            free: Some(free(870, 1000, NOW)),
            credit: Some(credit(10_000_000, 100_000, NOW)),
            credit_problem: Some(UsageProblem::PermissionDenied),
            ..Default::default()
        };
        assert_eq!(
            cli_line(&detail, WindowKind::Daily, NOW),
            "870 of 1000 free requests remain, 130 used, 87%"
        );
        assert_eq!(
            cli_line(&detail, WindowKind::Credit, NOW),
            "9.90$ balance, total 10.00$, used 0.10$"
        );
        let denied = OpenRouterDetail {
            credit_problem: Some(UsageProblem::PermissionDenied),
            ..Default::default()
        };
        assert!(cli_line(&denied, WindowKind::Credit, NOW).contains("PermissionDenied"));
    }
}

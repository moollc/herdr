use super::*;
use serde_json::json;
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

fn directory() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "herdr-usage-tests-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).expect("test dir");
    dir
}
pub(super) fn config() -> UsageConfig {
    let dir = directory();
    UsageConfig {
        codex_home: dir.join("codex"),
        claude_state_file: dir.join("claude.json"),
        snapshot_file: dir.join("usage.json"),
        fixture: None,
        ..UsageConfig::default()
    }
}
fn sample(name: &str) -> String {
    fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/usage")
            .join(name),
    )
    .expect("fixture")
}
fn log(config: &UsageConfig, name: &str, data: &str) -> PathBuf {
    let dir = config
        .codex_home
        .join("sessions")
        .join(local_time(1, c"%Y/%m/%d"));
    fs::create_dir_all(&dir).expect("log dir");
    let path = dir.join(name);
    fs::write(&path, data).expect("log");
    path
}
fn event(time: &str, used: Value, reset: u64) -> String {
    format!(
        "{}\n",
        json!({"timestamp":time,"type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"codex","primary":{"used_percent":used,"resets_at":reset},"secondary":{"used_percent":5,"resets_at":reset+600000}}}})
    )
}
#[test]
fn codex_synthetic_samples() {
    for name in ["normal", "high-use", "after-reset", "other-limit-id"] {
        let c = config();
        let data = sample(&format!("codex-token-count-{name}.jsonl"));
        log(&c, "rollout.jsonl", &data);
        let actual = codex::read(&c.codex_home, 1);
        if name == "other-limit-id" {
            assert_eq!(actual.problem, Some(UsageProblem::NoData));
            log(
                &c,
                "mixed.jsonl",
                &format!("{}{}", sample("codex-token-count-normal.jsonl"), data),
            );
            assert_eq!(codex::read(&c.codex_home, 1).problem, None);
            continue;
        }
        let expected: Value = data
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|v| {
                v.pointer("/payload/rate_limits/limit_id")
                    .and_then(Value::as_str)
                    == Some("codex")
            })
            .max_by_key(|v| v["timestamp"].as_str().unwrap_or("").to_owned())
            .expect("codex event");
        assert_eq!(actual.problem, None, "{name}");
        assert_eq!(
            actual.windows[0].left_pct,
            percent(&expected["payload"]["rate_limits"]["primary"]["used_percent"])
        );
        assert_eq!(
            actual.seen_at,
            expected["timestamp"].as_str().and_then(timestamp)
        );
    }
}
#[test]
fn newest_line_wins_and_reset_is_idle() {
    let c = config();
    log(
        &c,
        "older-file.jsonl",
        &event("2026-10-14T15:00:00Z", json!(90), 10),
    );
    log(
        &c,
        "newer-file.jsonl",
        &event("2026-10-14T14:00:00Z", json!(20), 9999999999),
    );
    let p = codex::read(&c.codex_home, 20);
    assert_eq!(p.windows[0].left_pct, Some(100));
    assert_eq!(p.windows[0].resets_at, Some(10));
    assert_eq!(p.seen_at, timestamp("2026-10-14T15:00:00Z"));
}
#[test]
fn codex_bounded_tail_widens_and_ignores_partial_lines() {
    let c = config();
    let data = format!(
        "{}{}\n{{broken",
        event("2026-10-14T15:00:00Z", json!(12.5), 9999999999),
        "x".repeat(70000)
    );
    let path = log(&c, "log.jsonl", &data);
    assert_eq!(codex::read(&c.codex_home, 1).windows[0].left_pct, Some(87));
    fs::write(
        path,
        format!(
            "{}{}",
            event("2026-10-14T15:00:00Z", json!(12), 9999999999),
            "x".repeat(300000)
        ),
    )
    .expect("huge");
    assert_eq!(
        codex::read(&c.codex_home, 1).problem,
        Some(UsageProblem::NoData)
    );
}
#[test]
fn codex_odd_and_missing_files() {
    let c = config();
    assert_eq!(
        codex::read(&c.codex_home, 1).problem,
        Some(UsageProblem::NoData)
    );
    for data in ["", "not json", "{\"type\":", "{\"rate_limits\":null}"] {
        log(&c, "log.jsonl", data);
        assert_eq!(
            codex::read(&c.codex_home, 1).problem,
            Some(UsageProblem::NoData)
        );
    }
    log(
        &c,
        "log.jsonl",
        &event("2026-10-14T15:00:00Z", json!("90"), 9999999999),
    );
    assert_eq!(
        codex::read(&c.codex_home, 1).problem,
        Some(UsageProblem::FormatChanged)
    );
    let path = c
        .codex_home
        .join("sessions")
        .join(local_time(1, c"%Y/%m/%d"))
        .join("directory.jsonl");
    fs::create_dir_all(path).expect("unreadable file substitute");
    assert!(codex::read(&c.codex_home, 1).windows[0].left_pct.is_none());
}
#[test]
fn codex_reads_at_most_five_newest_files() {
    let c = config();
    log(
        &c,
        "valid.jsonl",
        &event("2026-10-14T15:00:00Z", json!(90), 9999999999),
    );
    for n in 0..5 {
        log(&c, &format!("new{n}.jsonl"), "no events");
    }
    assert_eq!(
        codex::read(&c.codex_home, 1).problem,
        Some(UsageProblem::NoData)
    );
}
#[test]
fn claude_samples_before_and_after_turn() {
    for n in 1..=2 {
        let c = config();
        let input = sample(&format!("claude-statusline-input-redacted-{n}.json"));
        let mut out = Vec::new();
        statusline::run(&c, input.as_bytes(), &mut out, 100).expect("statusline");
        let actual = claude::read(&c.claude_state_file, 100);
        if n == 1 {
            assert!(!c.claude_state_file.exists());
            assert_eq!(actual.problem, Some(UsageProblem::NoData));
        } else {
            assert_eq!(actual.problem, None);
            assert_eq!(actual.windows[0].left_pct, Some(80));
            assert_eq!(actual.windows[1].left_pct, Some(97));
        }
        assert_eq!(String::from_utf8(out).expect("utf8").lines().count(), 1);
    }
}
#[test]
fn claude_optional_malformed_and_expired() {
    let c = config();
    for value in [
        json!({"seen_at":10}),
        json!({"seen_at":10,"seven_day":{"used_percentage":30,"resets_at":1}}),
        json!({"seen_at":10,"five_hour":null}),
    ] {
        atomic_json(&c.claude_state_file, &value).expect("save");
        let actual = claude::read(&c.claude_state_file, 10);
        if value.get("five_hour").is_some() {
            assert_eq!(actual.problem, Some(UsageProblem::FormatChanged));
        } else if value.get("seven_day").is_some() {
            assert_eq!(actual.windows[1].left_pct, Some(100));
            assert_eq!(actual.windows[0].left_pct, None);
        } else {
            assert_eq!(actual.problem, Some(UsageProblem::NoData));
        }
    }
    for bytes in ["", "{", "null", "[]", "not json"] {
        fs::write(&c.claude_state_file, bytes).expect("write");
        assert_eq!(
            claude::read(&c.claude_state_file, 10).problem,
            Some(UsageProblem::FormatChanged)
        );
    }
}
#[test]
fn statusline_keeps_newer_even_with_concurrent_writers() {
    let c = config();
    std::thread::scope(|scope| {
        for time in 1..=20 {
            let path = &c.claude_state_file;
            scope.spawn(move || {
                statusline::save_newer(path, &json!({"seen_at":time})).expect("save");
            });
        }
    });
    assert_eq!(
        read_json::<Value>(&c.claude_state_file).expect("state")["seen_at"],
        20
    );
    statusline::save_newer(&c.claude_state_file, &json!({"seen_at":20,"five_hour":{}}))
        .expect("equal");
    assert!(read_json::<Value>(&c.claude_state_file)
        .expect("state")
        .get("five_hour")
        .is_none());
}
#[test]
fn cursor_sample_missing_unlimited_and_idle() {
    let value: Value =
        serde_json::from_str(&sample("cursor-usage-summary.sample.json")).expect("sample");
    let p = cursor::parse(&value, 1);
    assert_eq!(p.problem, None);
    assert_eq!(p.windows[0].left_pct, Some(80));
    assert!(p.windows[0]
        .note
        .as_ref()
        .expect("note")
        .contains("overall 82% left, auto 80% left, named 99% left"));
    assert_eq!(
        cursor::parse(&value, u64::MAX).windows[0].left_pct,
        Some(100)
    );
    let mut single = value.clone();
    single["individualUsage"]["plan"]
        .as_object_mut()
        .expect("plan")
        .remove("autoPercentUsed");
    assert_eq!(cursor::parse(&single, 1).windows[0].left_pct, Some(99));
    single["individualUsage"]["plan"]
        .as_object_mut()
        .expect("plan")
        .remove("apiPercentUsed");
    assert_eq!(
        cursor::parse(&single, 1).problem,
        Some(UsageProblem::FormatChanged)
    );
    single["isUnlimited"] = json!(true);
    assert_eq!(cursor::parse(&single, 1).windows[0].left_pct, None);
    assert_eq!(cursor::parse(&single, 1).problem, None);
    assert_eq!(
        cursor::parse(&json!({"error":"not_authenticated"}), 1).problem,
        Some(UsageProblem::LoginExpired)
    );
}
#[test]
fn fixture_refresh_and_alert_jitter() {
    let mut c = config();
    let mut p = unknown(Provider::Codex, UsageProblem::NoData);
    p.problem = None;
    p.windows[0].left_pct = Some(10);
    p.windows[0].resets_at = Some(5000);
    let mut snap = UsageSnapshot {
        version: 1,
        taken_at: 1,
        providers: vec![p],
    };
    let path = c.snapshot_file.with_extension("fixture");
    atomic_json(&path, &snap).expect("fixture");
    c.fixture = Some(path);
    let mut state = UsageState::new(c);
    assert_eq!(state.refresh(&[Provider::Cursor], 100), snap);
    assert_eq!(
        state.take_alerts(&snap),
        vec![(Provider::Codex, WindowKind::FiveHour)]
    );
    assert!(state.take_alerts(&snap).is_empty());
    snap.providers[0].windows[0].resets_at = Some(5002);
    assert!(state.take_alerts(&snap).is_empty());
    snap.providers[0].windows[0].resets_at = Some(23000);
    assert_eq!(state.take_alerts(&snap).len(), 1);
    snap.taken_at = 24000;
    assert!(state.take_alerts(&snap).is_empty());
}
#[test]
fn refresh_only_wanted_providers_and_snapshot() {
    let c = config();
    let path = c.snapshot_file.clone();
    let mut state = UsageState::new(c);
    let snap = state.refresh(&[Provider::Claude, Provider::Claude], 100);
    assert_eq!(snap.providers.len(), 1);
    assert_eq!(read_json::<UsageSnapshot>(&path).expect("snapshot"), snap);
    assert_eq!(state.last_cursor_fetch, None);
}
#[test]
fn cursor_cached_polling_and_offline_reading() {
    let c = config();
    let mut state = UsageState::new(c);
    let p = cursor::parse(
        &serde_json::from_str(&sample("cursor-usage-summary.sample.json")).expect("sample"),
        1000,
    );
    state.cursor_result = Some(p.clone());
    state.last_cursor_fetch = Some(1000);
    assert_eq!(state.refresh(&[Provider::Cursor], 1899).providers, vec![p]);
    assert_eq!(state.last_cursor_fetch, Some(1000));
    // No live fetch is made by this test.
}
#[test]
fn palette_thresholds_and_percentage_clamping() {
    assert_eq!(
        palette::level_rgb(Provider::Codex, WindowKind::Weekly, Some(10)),
        palette::RED
    );
    assert_eq!(
        palette::level_rgb(Provider::Codex, WindowKind::Weekly, Some(20)),
        palette::PINK
    );
    assert_eq!(percent(&json!(-50)), Some(100));
    assert_eq!(percent(&json!(200)), Some(0));
    assert_eq!(timestamp("2026-02-31T00:00:00Z"), None);
    assert_eq!(timestamp("bad"), None);
}
#[test]
fn refresh_speed_100_runs() {
    let c = config();
    log(&c, "log.jsonl", &sample("codex-token-count-normal.jsonl"));
    let mut output = Vec::new();
    statusline::run(
        &c,
        sample("claude-statusline-input-redacted-2.json").as_bytes(),
        &mut output,
        1,
    )
    .expect("statusline");
    let mut state = UsageState::new(c);
    let mut times = Vec::new();
    for _ in 0..100 {
        let start = Instant::now();
        let snap = state.refresh(&[Provider::Claude, Provider::Codex], 1);
        assert_eq!(snap.providers.len(), 2);
        times.push(start.elapsed().as_micros());
    }
    times.sort_unstable();
    println!(
        "USAGE_REFRESH_100 median_us={} max_us={}",
        times[50], times[99]
    );
}

#[test]
fn cursor_http_failures_never_guess_login() {
    assert_eq!(
        cursor::http_result(401, "", "", 1).problem,
        Some(UsageProblem::LoginExpired)
    );
    assert_eq!(
        cursor::http_result(
            307,
            "https://api.workos.com/user_management/authorize",
            "",
            1
        )
        .problem,
        Some(UsageProblem::LoginExpired)
    );
    assert_eq!(
        cursor::http_result(403, "", "", 1).problem,
        Some(UsageProblem::FormatChanged)
    );
    assert_eq!(
        cursor::http_result(500, "", "", 1).problem,
        Some(UsageProblem::Offline)
    );
    assert_eq!(
        cursor::http_result(200, "", "broken", 1).problem,
        Some(UsageProblem::FormatChanged)
    );
    assert_eq!(
        cursor::http_result(200, "", r#"{"error":"not_authenticated"}"#, 1).problem,
        Some(UsageProblem::LoginExpired)
    );
}
#[test]
fn cursor_offline_keeps_good_numbers_and_throttles_errors() {
    let mut c = config();
    c.cursor_min_interval_secs = 1;
    let mut state = UsageState::new(c);
    let good = cursor::http_result(200, "", &sample("cursor-usage-summary.sample.json"), 1000);
    assert_eq!(state.refresh_cursor(1000, |_| good.clone()), good);
    assert_eq!(
        state.refresh_cursor(1899, |_| panic!("must not fetch early")),
        good
    );
    let offline = state.refresh_cursor(1900, |_| unknown(Provider::Cursor, UsageProblem::Offline));
    assert_eq!(offline.problem, Some(UsageProblem::Offline));
    assert_eq!(offline.windows, good.windows);
    assert_eq!(offline.seen_at, good.seen_at);
    let _ = state.refresh_cursor(2800, |_| {
        unknown(Provider::Cursor, UsageProblem::LoginExpired)
    });
    let offline = state.refresh_cursor(3700, |_| unknown(Provider::Cursor, UsageProblem::Offline));
    assert_eq!(offline.problem, Some(UsageProblem::LoginExpired));
    assert_eq!(offline.windows[0].left_pct, None);
    assert!(state.cursor_good.is_none());
}
#[test]
fn timestamp_dates_and_fractions() {
    assert_eq!(timestamp("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(timestamp("2026-11-08T05:21:26.000Z"), Some(1794115286));
    assert_eq!(timestamp("2000-02-29T00:00:00Z"), Some(951782400));
    for s in [
        "2100-02-29T00:00:00Z",
        "2026-10-14T00:00:00.Z",
        "2026-13-01T00:00:00Z",
        "2026-10-14T24:00:00Z",
        "2026-10-14T00:00:00+00:00",
    ] {
        assert_eq!(timestamp(s), None, "{s}");
    }
}

#[test]
fn codex_subsecond_order_and_bad_timestamp() {
    let c = config();
    log(
        &c,
        "a.jsonl",
        &event("2026-10-14T15:00:00.950Z", json!(90), 9999999999),
    );
    log(
        &c,
        "b.jsonl",
        &event("2026-10-14T15:00:00.100Z", json!(20), 9999999999),
    );
    assert_eq!(codex::read(&c.codex_home, 1).windows[0].left_pct, Some(10));
    let c = config();
    log(&c, "bad.jsonl", &event("bad", json!(90), 9999999999));
    assert_eq!(
        codex::read(&c.codex_home, 1).problem,
        Some(UsageProblem::FormatChanged)
    );
}
#[test]
fn codex_unreadable_file() {
    use std::os::unix::fs::PermissionsExt;
    let c = config();
    let path = log(
        &c,
        "log.jsonl",
        &event("2026-10-14T15:00:00Z", json!(90), 9999999999),
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o0)).expect("permissions");
    let result = codex::read(&c.codex_home, 1);
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("restore permissions");
    assert_eq!(result.problem, Some(UsageProblem::NoData));
}

#[test]
fn refresh_live_speed_100_runs() {
    let mut c = config();
    c.codex_home = UsageConfig::default().codex_home;
    let mut output = Vec::new();
    statusline::run(
        &c,
        sample("claude-statusline-input-redacted-2.json").as_bytes(),
        &mut output,
        now(),
    )
    .expect("statusline");
    let mut state = UsageState::new(c);
    let mut times = Vec::new();
    for _ in 0..100 {
        let start = Instant::now();
        let snap = state.refresh(&[Provider::Claude, Provider::Codex], now());
        assert_eq!(snap.providers.len(), 2);
        times.push(start.elapsed().as_micros());
    }
    times.sort_unstable();
    println!(
        "USAGE_LIVE_REFRESH_100 median_us={} max_us={}",
        times[50], times[99]
    );
}

#[test]
fn codex_empty_today_falls_back_one_folder() {
    let c = config();
    fs::create_dir_all(c.codex_home.join("sessions/2026/10/15")).expect("empty today");
    log(
        &c,
        "yesterday.jsonl",
        &event("2026-10-14T15:00:00Z", json!(32), 9999999999),
    );
    let yesterday = c.codex_home.join("sessions/2026/10/14");
    fs::create_dir_all(&yesterday).expect("yesterday");
    fs::write(
        yesterday.join("log.jsonl"),
        event("2026-10-14T15:00:00Z", json!(32), 9999999999),
    )
    .expect("yesterday log");
    let actual = codex::read(
        &c.codex_home,
        timestamp("2026-10-15T15:00:00Z").expect("date"),
    );
    assert_eq!(actual.windows[0].left_pct, Some(68));
}

#[test]
fn cli_refreshes_each_minute_without_postponing_cursor_fetch() {
    let c = config();
    let good = cursor::http_result(200, "", &sample("cursor-usage-summary.sample.json"), 1000);
    UsageState::new(c.clone())
        .refresh_with_cursor_fetch(&[Provider::Cursor], 1000, |_| good.clone());
    let mut attempts = Vec::new();
    for minute in 1..=20 {
        let now = 1000 + minute * 60;
        let want = cli::wanted_providers(&c, now);
        let mut state = UsageState::new(c.clone());
        let snapshot = state.refresh_with_cursor_fetch(&want, now, |_| {
            attempts.push(now);
            good.clone()
        });
        assert!(snapshot
            .providers
            .iter()
            .any(|p| p.provider == Provider::Cursor));
        assert_eq!(
            cursor_fetch_time(&c),
            Some(if minute < 15 { 1000 } else { 1900 })
        );
    }
    assert_eq!(attempts, vec![1900]);
}
#[test]
fn claude_only_refresh_preserves_cursor_and_fetch_time() {
    let c = config();
    let good = cursor::http_result(200, "", &sample("cursor-usage-summary.sample.json"), 1000);
    let mut state = UsageState::new(c.clone());
    state.refresh_with_cursor_fetch(&[Provider::Cursor], 1000, |_| good.clone());
    let snapshot =
        state.refresh_with_cursor_fetch(&[Provider::Claude], 1200, |_| panic!("Cursor not wanted"));
    assert_eq!(
        snapshot
            .providers
            .iter()
            .find(|p| p.provider == Provider::Cursor),
        Some(&good)
    );
    assert_eq!(
        read_json::<UsageSnapshot>(&c.snapshot_file).expect("saved"),
        snapshot
    );
    assert_eq!(cursor_fetch_time(&c), Some(1000));
}
#[test]
fn cursor_due_uses_sidecar_and_shared_configured_interval() {
    let mut c = config();
    c.cursor_min_interval_secs = 1200;
    let mut state = UsageState::new(c.clone());
    state.refresh_with_cursor_fetch(&[Provider::Cursor], 1000, |_| {
        unknown(Provider::Cursor, UsageProblem::NoLogin)
    });
    assert!(!cursor_fetch_due(&c, 2199));
    assert!(cursor_fetch_due(&c, 2200));
    assert!(!cli::wanted_providers(&c, 2199).contains(&Provider::Cursor));
    assert!(cli::wanted_providers(&c, 2200).contains(&Provider::Cursor));
    let mut reloaded = UsageState::new(c.clone());
    assert_eq!(reloaded.last_cursor_fetch, Some(1000));
    reloaded.refresh_cursor(2199, |_| panic!("not due"));
    reloaded.refresh_cursor(2200, |_| unknown(Provider::Cursor, UsageProblem::NoLogin));
    assert_eq!(cursor_fetch_time(&c), Some(2200));
    let absent = config();
    assert!(cursor_fetch_due(&absent, 1));
    fs::create_dir_all(cursor_fetch_file(&absent)).expect("unreadable sidecar");
    assert!(cursor_fetch_due(&absent, 1));
    assert_eq!(UsageState::new(absent).last_cursor_fetch, None);
}
#[test]
fn login_problems_stick_after_restart_and_offline() {
    for problem in [UsageProblem::LoginExpired, UsageProblem::NoLogin] {
        let c = config();
        let good = cursor::http_result(200, "", &sample("cursor-usage-summary.sample.json"), 1000);
        let mut state = UsageState::new(c.clone());
        state.refresh_with_cursor_fetch(&[Provider::Cursor], 1000, |_| good.clone());
        state.refresh_with_cursor_fetch(&[Provider::Cursor], 1900, |_| {
            unknown(Provider::Cursor, problem.clone())
        });
        assert!(state.cursor_good.is_none());
        let mut state = UsageState::new(c);
        let snap = state.refresh_with_cursor_fetch(&[Provider::Cursor], 2800, |_| {
            unknown(Provider::Cursor, UsageProblem::Offline)
        });
        assert_eq!(snap.providers[0].problem, Some(problem));
        assert_eq!(snap.providers[0].windows[0].left_pct, None);
    }
}
#[test]
fn all_providers_floor_fractional_percentages() {
    let c = config();
    log(
        &c,
        "fraction.jsonl",
        &event("2026-10-14T15:00:00Z", json!(12.5), 9999999999),
    );
    assert_eq!(codex::read(&c.codex_home, 1).windows[0].left_pct, Some(87));
    let claude = claude::parse(
        &json!({"seen_at":1,"five_hour":{"used_percentage":12.5,"resets_at":9999999999u64}}),
        1,
    );
    assert_eq!(claude.windows[0].left_pct, Some(87));
    for (used, left) in [(0.1, 99), (79.9, 20), (89.9, 10), (99.9, 0)] {
        assert_eq!(percent(&json!(used)), Some(left));
    }
}
#[test]
fn codex_premium_only_today_falls_back_with_five_file_budget() {
    let c = config();
    let previous = c.codex_home.join("sessions/2026/10/13");
    fs::create_dir_all(&previous).expect("previous");
    fs::write(
        previous.join("codex.jsonl"),
        event("2026-10-13T15:00:00Z", json!(12.5), 9999999999),
    )
    .expect("older event");
    let today = c.codex_home.join("sessions/2026/10/14");
    fs::create_dir_all(&today).expect("today");
    for n in 0..5 {
        fs::write(
            today.join(format!("premium-{n}.jsonl")),
            sample("codex-token-count-other-limit-id.jsonl"),
        )
        .expect("premium");
    }
    let result = codex::read(
        &c.codex_home,
        timestamp("2026-10-14T15:00:00Z").expect("today"),
    );
    assert_eq!(result.problem, None);
    assert_eq!(result.windows[0].left_pct, Some(87));
    assert_eq!(result.seen_at, timestamp("2026-10-13T15:00:00Z"));
}

#[test]
fn importer_exit_codes_and_single_immediate_retry() {
    use login::CursorLoginResult::Exited;
    for code in [0, 1, 3, 4, 9] {
        let c = config();
        let mut state = UsageState::new(c.clone());
        let good = cursor::http_result(200, "", &sample("cursor-usage-summary.sample.json"), 1000);
        let mut reads = 0;
        let mut commands = 0;
        let result = state.refresh_cursor_with_login(
            1000,
            |_| {
                reads += 1;
                if reads == 1 {
                    unknown(Provider::Cursor, UsageProblem::NoLogin)
                } else {
                    good.clone()
                }
            },
            |timeout| {
                assert_eq!(timeout, std::time::Duration::from_secs(20));
                commands += 1;
                Exited(code)
            },
        );
        assert_eq!(commands, 1);
        assert_eq!(reads, if code == 0 { 2 } else { 1 });
        assert_eq!(
            result.problem,
            match code {
                0 => None,
                4 => Some(UsageProblem::LoginExpired),
                _ => Some(UsageProblem::NoLogin),
            }
        );
        assert_eq!(state.cursor_login.no_consent, code == 3);
        assert_eq!(
            read_json::<u64>(&c.snapshot_file.with_extension("cursor-login")).expect("attempt"),
            1000
        );
    }
    let mut state = UsageState::new(config());
    let mut reads = 0;
    let mut commands = 0;
    let result = state.refresh_cursor_with_login(
        1000,
        |_| {
            reads += 1;
            unknown(Provider::Cursor, UsageProblem::LoginExpired)
        },
        |_| {
            commands += 1;
            Exited(0)
        },
    );
    assert_eq!(reads, 2);
    assert_eq!(commands, 1);
    assert_eq!(result.problem, Some(UsageProblem::LoginExpired));
}
#[test]
fn importer_hour_limit_survives_new_process_and_stale_memory() {
    use login::CursorLoginResult::Exited;
    let c = config();
    let mut commands = 0;
    let mut stale = login::RefreshState::new(&c);
    for (now, expected) in [(1000, 1), (1900, 1), (4600, 2)] {
        let mut state = UsageState::new(c.clone());
        state.refresh_with_sources(
            &[Provider::Cursor],
            now,
            |_| unknown(Provider::Cursor, UsageProblem::LoginExpired),
            |_| {
                commands += 1;
                Exited(1)
            },
        );
        assert_eq!(commands, expected);
    }
    assert_eq!(
        stale.attempt(&c, 5500, |_| panic!("persisted hourly limit")),
        None
    );
    assert_eq!(
        stale.attempt(&c, 8199, |_| panic!("one second before hour")),
        None
    );
    assert_eq!(stale.attempt(&c, 8200, |_| Exited(4)), Some(Exited(4)));
}
#[test]
fn importer_no_consent_stays_no_login_for_this_process() {
    use login::CursorLoginResult::Exited;
    let mut state = UsageState::new(config());
    let result = state.refresh_cursor_with_login(
        1000,
        |_| unknown(Provider::Cursor, UsageProblem::LoginExpired),
        |_| Exited(3),
    );
    assert_eq!(result.problem, Some(UsageProblem::NoLogin));
    let result = state.refresh_cursor_with_login(
        90000,
        |_| unknown(Provider::Cursor, UsageProblem::LoginExpired),
        |_| panic!("no consent never retries"),
    );
    assert_eq!(result.problem, Some(UsageProblem::NoLogin));
}
#[test]
fn importer_timeout_and_command_failure_keep_problem_and_hour_limit() {
    use login::CursorLoginResult;
    for outcome in [
        CursorLoginResult::TimedOut,
        CursorLoginResult::Failed,
        CursorLoginResult::Exited(1),
    ] {
        let c = config();
        let mut state = UsageState::new(c.clone());
        let result = state.refresh_cursor_with_login(
            1000,
            |_| unknown(Provider::Cursor, UsageProblem::LoginExpired),
            |_| outcome,
        );
        assert_eq!(result.problem, Some(UsageProblem::LoginExpired));
        let mut state = UsageState::new(c);
        state.refresh_cursor_with_login(
            1900,
            |_| unknown(Provider::Cursor, UsageProblem::LoginExpired),
            |_| panic!("failed attempt still hourly"),
        );
    }
}
#[test]
fn importer_only_runs_for_login_problems_and_never_for_unwanted_cursor() {
    for problem in [
        UsageProblem::Offline,
        UsageProblem::FormatChanged,
        UsageProblem::NoData,
    ] {
        let mut state = UsageState::new(config());
        state.refresh_cursor_with_login(
            1000,
            |_| unknown(Provider::Cursor, problem.clone()),
            |_| panic!("not a login error"),
        );
    }
    let good = cursor::http_result(200, "", &sample("cursor-usage-summary.sample.json"), 1000);
    let mut state = UsageState::new(config());
    state.refresh_cursor_with_login(1000, |_| good.clone(), |_| panic!("healthy login"));
    state.refresh_with_sources(
        &[Provider::Claude],
        2000,
        |_| panic!("Cursor not wanted"),
        |_| panic!("Cursor not wanted"),
    );
}
#[test]
fn successful_import_then_offline_keeps_known_login_error() {
    use login::CursorLoginResult::Exited;
    let mut state = UsageState::new(config());
    let good = cursor::http_result(200, "", &sample("cursor-usage-summary.sample.json"), 1);
    state.cursor_good = Some(good.clone());
    state.cursor_result = Some(good);
    let mut reads = 0;
    let result = state.refresh_cursor_with_login(
        1000,
        |_| {
            reads += 1;
            unknown(
                Provider::Cursor,
                if reads == 1 {
                    UsageProblem::LoginExpired
                } else {
                    UsageProblem::Offline
                },
            )
        },
        |_| Exited(0),
    );
    assert_eq!(reads, 2);
    assert_eq!(result.problem, Some(UsageProblem::LoginExpired));
    assert_eq!(result.windows[0].left_pct, None);
}
#[test]
fn cli_login_hints_follow_cursor_line() {
    for (problem, hint) in [
        (UsageProblem::NoLogin, "run: herdr usage cursor-login"),
        (
            UsageProblem::LoginExpired,
            "sign in to cursor.com in your browser",
        ),
    ] {
        let snapshot = UsageSnapshot {
            version: 1,
            taken_at: 1000,
            providers: vec![unknown(Provider::Cursor, problem)],
        };
        let mut out = Vec::new();
        cli::write_plain(&snapshot, 1000, &mut out).expect("print");
        let text = String::from_utf8(out).expect("utf8");
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("cursor monthly"));
        assert_eq!(lines[1], hint);
    }
}
#[test]
fn two_process_states_share_one_hourly_import_attempt() {
    use login::CursorLoginResult::Exited;
    let c = config();
    let count = std::sync::atomic::AtomicUsize::new(0);
    let states = [login::RefreshState::new(&c), login::RefreshState::new(&c)];
    std::thread::scope(|scope| {
        for mut state in states {
            let c = &c;
            let count = &count;
            scope.spawn(move || {
                state.attempt(c, 1000, |_| {
                    count.fetch_add(1, Ordering::SeqCst);
                    Exited(1)
                });
            });
        }
    });
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[test]
fn usage_worker_publishes_before_slow_importer_and_again_after_success() {
    use std::sync::mpsc;
    use std::time::Duration;
    let config = config();
    atomic_json(
        &config.claude_state_file,
        &json!({"seen_at": 1000,
        "five_hour": {"used_percentage": 20, "resets_at": 3000},
        "seven_day": {"used_percentage": 30, "resets_at": 9000}}),
    )
    .expect("Claude reading");
    log(
        &config,
        "rollout.jsonl",
        &event("1970-01-01T00:16:40Z", json!(25), 3000),
    );
    let (updates_tx, updates_rx) = mpsc::channel();
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let child = std::thread::spawn(move || {
        let mut state = UsageState::new(config);
        let mut reads = 0;
        let snapshot = state.refresh_with_sources_and_publish(
            &[Provider::Claude, Provider::Codex, Provider::Cursor],
            1000,
            |_| {
                reads += 1;
                if reads == 1 {
                    unknown(Provider::Cursor, UsageProblem::NoLogin)
                } else {
                    cursor::http_result(200, "", &sample("cursor-usage-summary.sample.json"), 1000)
                }
            },
            true,
            |_| {
                started_tx.send(()).expect("child started");
                release_rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("release slow child");
                login::CursorLoginResult::Exited(0)
            },
            |snapshot| updates_tx.send(snapshot).expect("snapshot event"),
        );
        done_tx.send((snapshot, reads)).expect("finished");
    });
    let first = updates_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("first update");
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("login started");
    assert_eq!(
        first
            .providers
            .iter()
            .map(|p| p.provider)
            .collect::<Vec<_>>(),
        vec![Provider::Claude, Provider::Codex, Provider::Cursor]
    );
    assert_eq!(first.providers[0].windows[0].left_pct, Some(80));
    assert_eq!(first.providers[1].windows[0].left_pct, Some(75));
    assert_eq!(first.providers[2].problem, Some(UsageProblem::NoLogin));
    assert!(done_rx.try_recv().is_err(), "worker is still in the child");
    assert!(
        updates_rx.try_recv().is_err(),
        "no second update before child exits"
    );
    release_tx.send(()).expect("finish child");
    let second = updates_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("Cursor update");
    let (final_snapshot, reads) = done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("worker done");
    child.join().expect("worker joined");
    assert_eq!(reads, 2, "exactly one immediate Cursor refetch");
    assert_eq!(second.providers[2].problem, None);
    assert!(second.providers[2].windows[0].left_pct.is_some());
    assert_eq!(
        &second.providers[..2],
        &first.providers[..2],
        "local readings preserved"
    );
    assert_eq!(final_snapshot, second);
    assert!(updates_rx.try_recv().is_err(), "only two snapshot updates");
}

#[test]
fn worker_no_consent_preserves_hour_and_grant_imports_on_next_refresh() {
    use login::CursorLoginResult::Exited;
    let c = config();
    let marker = c.snapshot_file.with_extension("cursor-login");
    let mut state = UsageState::new(c.clone());
    let snapshot = state.refresh_worker_with_sources(
        &[Provider::Cursor],
        1000,
        false,
        |_| unknown(Provider::Cursor, UsageProblem::NoLogin),
        |_| panic!("no consent must not run importer"),
        |_| {},
    );
    assert_eq!(snapshot.providers[0].problem, Some(UsageProblem::NoLogin));
    assert!(state.cursor_login.no_consent);
    assert!(
        !marker.exists(),
        "no consent must not create the hourly marker"
    );
    assert_eq!(
        state
            .cursor_login
            .attempt(&c, 1001, |_| panic!("latched no consent")),
        None
    );
    assert!(!marker.exists());
    let good = cursor::http_result(200, "", &sample("cursor-usage-summary.sample.json"), 1300);
    let mut reads = 0;
    let mut imports = 0;
    let snapshot = state.refresh_worker_with_sources(
        &[Provider::Cursor],
        1300,
        true,
        |_| {
            reads += 1;
            if reads == 1 {
                unknown(Provider::Cursor, UsageProblem::NoLogin)
            } else {
                good.clone()
            }
        },
        |_| {
            imports += 1;
            Exited(0)
        },
        |_| {},
    );
    assert_eq!(
        imports, 1,
        "grant imports on the next refresh, before the hour or poll interval"
    );
    assert_eq!(reads, 2);
    assert!(!state.cursor_login.no_consent);
    assert_eq!(snapshot.providers[0], good);
    assert_eq!(
        read_json::<u64>(&marker).expect("real attempt marker"),
        1300
    );
    let mut reloaded = login::RefreshState::new(&c);
    assert_eq!(
        reloaded.attempt(&c, 1301, |_| panic!("real attempt is still hourly")),
        None
    );
}

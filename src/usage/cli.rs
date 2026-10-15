use super::{
    countdown::countdown, local_time, read_json, Provider, UsageConfig, UsageSnapshot, UsageState,
};
use std::io;
pub fn run(args: &[String]) -> io::Result<()> {
    let config = UsageConfig::configured();
    if args.first().map(String::as_str) == Some("cursor-login") {
        let code = crate::platform::usage::cursor_login_cli(&args[1..]);
        if code != 0 {
            std::process::exit(code);
        }
        return Ok(());
    }
    let now = super::now();
    if args.first().map(String::as_str) == Some("claude-statusline") && args.len() == 1 {
        return super::statusline::run(&config, io::stdin().lock(), &mut io::stdout().lock(), now);
    }
    if args.len() > 1 || args.first().is_some_and(|arg| arg != "--json") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: herdr usage [--json | claude-statusline | cursor-login [--refresh | --status | --forget]]",
        ));
    }
    run_readings(
        !args.is_empty(),
        config,
        now,
        |service| super::cursor::fetch(service, now),
        |_| unreachable!("CLI must not run a login command"),
        &mut io::stdout().lock(),
    )
}
// Keep the CLI policy in this shared, injectable path for both plain and JSON output.
fn run_readings(
    json: bool,
    config: UsageConfig,
    now: u64,
    fetch: impl FnMut(&str) -> super::ProviderUsage,
    command: impl FnOnce(std::time::Duration) -> super::login::CursorLoginResult,
    output: &mut impl io::Write,
) -> io::Result<()> {
    let want = wanted_providers(&config, now);
    let mut state = UsageState::new(config);
    let snapshot =
        state.refresh_with_sources_and_publish(&want, now, fetch, false, command, |_| {});
    if json {
        writeln!(output, "{}", serde_json::to_string_pretty(&snapshot)?)
    } else {
        write_plain(&snapshot, now, output)
    }
}
pub(super) fn write_plain(
    snapshot: &UsageSnapshot,
    now: u64,
    output: &mut impl io::Write,
) -> io::Result<()> {
    for p in &snapshot.providers {
        for w in &p.windows {
            let left = w.left_pct.map_or("?".into(), |n| format!("{n}%"));
            let reset = w
                .resets_at
                .map_or("?".into(), |t| local_time(t, c"%Y-%m-%d %H:%M:%S %Z"));
            let age = p
                .seen_at
                .map_or("?".into(), |t| format!("{}s", now.saturating_sub(t)));
            let provider = serde_json::to_string(&p.provider)?;
            let kind = serde_json::to_string(&w.kind)?;
            let problem = p
                .problem
                .as_ref()
                .map_or("none".into(), |p| format!("{p:?}"));
            writeln!(
                output,
                "{} {} {left} left {} reset {reset} source={} age={age} problem={problem}{}",
                provider.trim_matches('"'),
                kind.trim_matches('"'),
                countdown(w.resets_at, now).text,
                p.source,
                w.note.as_ref().map_or(String::new(), |n| format!(" {n}"))
            )?;
        }
        if p.provider == Provider::Cursor {
            match p.problem {
                Some(super::UsageProblem::NoLogin) => {
                    writeln!(output, "run: herdr usage cursor-login")?
                }
                Some(super::UsageProblem::LoginExpired) => {
                    writeln!(output, "sign in to cursor.com in your browser")?
                }
                _ => {}
            }
        }
    }
    Ok(())
}

pub(super) fn wanted_providers(config: &UsageConfig, now: u64) -> Vec<Provider> {
    let mut want = vec![Provider::Claude, Provider::Codex];
    let cursor_wanted = read_json::<UsageSnapshot>(&config.snapshot_file)
        .ok()
        .is_some_and(|s| s.providers.iter().any(|p| p.provider == Provider::Cursor));
    if cursor_wanted && super::cursor_fetch_due(config, now) {
        want.push(Provider::Cursor);
    }
    want
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{atomic_json, tests::config, unknown, UsageProblem};

    #[test]
    fn cli_plain_and_json_never_call_injected_login_or_spend_the_hour() {
        for json in [false, true] {
            let config = config();
            let marker = config.snapshot_file.with_extension("cursor-login");
            let original = UsageSnapshot {
                version: 1,
                taken_at: 1,
                providers: vec![unknown(Provider::Cursor, UsageProblem::NoLogin)],
            };
            atomic_json(&config.snapshot_file, &original).expect("seed Cursor presence");
            let mut output = Vec::new();
            let mut reads = 0;
            run_readings(
                json,
                config,
                1000,
                |_| {
                    reads += 1;
                    unknown(Provider::Cursor, UsageProblem::NoLogin)
                },
                |_| panic!("CLI ran importer"),
                &mut output,
            )
            .expect("CLI output");
            assert_eq!(reads, 1, "the allowance fetch is due");
            assert!(
                !marker.exists(),
                "CLI must not spend the app's hourly attempt"
            );
            if json {
                let snapshot: UsageSnapshot = serde_json::from_slice(&output).expect("JSON only");
                assert_eq!(
                    snapshot.providers.last().expect("Cursor").problem,
                    Some(UsageProblem::NoLogin)
                );
            } else {
                assert!(String::from_utf8(output)
                    .expect("text")
                    .contains("run: herdr usage cursor-login"));
            }
        }
    }
}

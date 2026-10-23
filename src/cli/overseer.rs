use crate::api::schema::{Method, OverseerEscalateParams, OverseerMessageParams, Request};

pub(super) fn run_overseer_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(|arg| arg.as_str()) {
        Some("escalate") => escalate(&args[1..]),
        Some("message") => message(&args[1..]),
        Some("help" | "--help" | "-h") => {
            print_overseer_help();
            Ok(0)
        }
        _ => {
            print_overseer_help();
            Ok(2)
        }
    }
}

/// The pane this command runs in. Overseer commands act for it and nothing else.
fn caller_pane() -> Option<String> {
    std::env::var(crate::integration::HERDR_PANE_ID_ENV_VAR)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn escalation_args(args: &[String]) -> (bool, &[String]) {
    match args.first().map(String::as_str) {
        Some("--fyi") => (false, &args[1..]),
        Some("--wake") => (true, &args[1..]),
        _ => (true, args),
    }
}

fn escalate(args: &[String]) -> std::io::Result<i32> {
    let (wake, args) = escalation_args(args);
    if args.is_empty() {
        eprintln!("usage: herdr overseer escalate [--wake|--fyi] <text>");
        return Ok(2);
    }
    let Some(caller_pane_id) = caller_pane() else {
        eprintln!("herdr overseer escalate must run inside a Herdr pane");
        return Ok(2);
    };
    super::print_response(&super::send_request(&Request {
        id: "cli:overseer:escalate".into(),
        method: Method::OverseerEscalate(OverseerEscalateParams {
            caller_pane_id,
            text: args.join(" "),
            wake,
        }),
    })?)
}

fn message(args: &[String]) -> std::io::Result<i32> {
    if args.len() < 2 {
        eprintln!("usage: herdr overseer message <pane> <text>");
        return Ok(2);
    }
    let Some(caller_pane_id) = caller_pane() else {
        eprintln!("herdr overseer message must run inside a Herdr pane");
        return Ok(2);
    };
    super::print_response(&super::send_request(&Request {
        id: "cli:overseer:message".into(),
        method: Method::OverseerMessage(OverseerMessageParams {
            caller_pane_id,
            to: super::normalize_pane_id(&args[0]),
            text: args[1..].join(" "),
        }),
    })?)
}

fn print_overseer_help() {
    eprintln!("herdr overseer commands:");
    eprintln!(
        "  herdr overseer escalate [--wake|--fyi] <text>  wake by default; --fyi updates the situation only"
    );
    eprintln!(
        "  herdr overseer message <pane> <text>   the chief sends one line to a space overseer"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escalation_wakes_unless_explicitly_information() {
        for (args, wake, text) in [
            (vec!["status"], true, "status"),
            (vec!["--wake", "decision"], true, "decision"),
            (vec!["--fyi", "running"], false, "running"),
            (vec!["unknown", "line"], true, "unknown line"),
        ] {
            let args: Vec<String> = args.into_iter().map(String::from).collect();
            let (actual, rest) = escalation_args(&args);
            assert_eq!(actual, wake);
            assert_eq!(rest.join(" "), text);
        }
    }
}

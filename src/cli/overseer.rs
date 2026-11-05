use crate::api::schema::{
    ManagedRole, Method, OverseerEscalateParams, OverseerMessageParams, Request,
    RoleHandoffRecordParams, RoleMemoryAppendParams, RoleSetParams, RoleStatusParams,
};

pub(super) fn run_overseer_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(|arg| arg.as_str()) {
        Some("set") => set_role(ManagedRole::SpaceOverseer, &args[1..], true),
        Some("release") => set_role(ManagedRole::SpaceOverseer, &args[1..], false),
        Some("status") => role_status(&args[1..]),
        Some("memory") => memory_append(ManagedRole::SpaceOverseer, &args[1..]),
        Some("handoff") => handoff_record(ManagedRole::SpaceOverseer, &args[1..]),
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

pub(super) fn run_overlord_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(|arg| arg.as_str()) {
        Some("set") => set_role(ManagedRole::SessionOverlord, &args[1..], true),
        Some("release") => set_role(ManagedRole::SessionOverlord, &args[1..], false),
        Some("status") => role_status(&args[1..]),
        Some("memory") => memory_append(ManagedRole::SessionOverlord, &args[1..]),
        Some("handoff") => handoff_record(ManagedRole::SessionOverlord, &args[1..]),
        Some("message") => message(&args[1..]),
        Some("help" | "--help" | "-h") => {
            print_overlord_help();
            Ok(0)
        }
        _ => {
            print_overlord_help();
            Ok(2)
        }
    }
}

fn set_role(role: ManagedRole, args: &[String], enabled: bool) -> std::io::Result<i32> {
    if args.len() != 1 {
        eprintln!(
            "usage: herdr {} {} <pane>",
            role_command(role),
            if enabled { "set" } else { "release" }
        );
        return Ok(2);
    }
    let Some(caller_pane_id) = caller_pane() else {
        eprintln!("role assignment must run inside a Herdr pane");
        return Ok(2);
    };
    super::print_response(&super::send_request(&Request {
        id: format!(
            "cli:role:{}:{}",
            role_command(role),
            if enabled { "set" } else { "release" }
        ),
        method: Method::RoleSet(RoleSetParams {
            caller_pane_id,
            target_pane_id: Some(super::normalize_pane_id(&args[0])),
            role,
            enabled,
        }),
    })?)
}

fn role_status(args: &[String]) -> std::io::Result<i32> {
    let Some(caller_pane_id) = caller_pane() else {
        eprintln!("role status must run inside a Herdr pane");
        return Ok(2);
    };
    let mut inspected_pane_id = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--pane" if index + 1 < args.len() && inspected_pane_id.is_none() => {
                inspected_pane_id = Some(super::normalize_pane_id(&args[index + 1]));
                index += 2;
            }
            "--json" => index += 1,
            _ => {
                eprintln!("usage: herdr overseer status [--pane TARGET] [--json]");
                return Ok(2);
            }
        }
    }
    super::print_response(&super::send_request(&Request {
        id: "cli:role:status".into(),
        method: Method::RoleStatus(RoleStatusParams {
            caller_pane_id,
            inspected_pane_id,
        }),
    })?)
}

fn memory_append(role: ManagedRole, args: &[String]) -> std::io::Result<i32> {
    if args.len() < 2 || args[0] != "append" {
        eprintln!("usage: herdr {} memory append <text>", role_command(role));
        return Ok(2);
    }
    let Some(caller_pane_id) = caller_pane() else {
        eprintln!("role memory append must run inside a Herdr pane");
        return Ok(2);
    };
    super::print_response(&super::send_request(&Request {
        id: format!("cli:{}:memory:append", role_command(role)),
        method: Method::RoleMemoryAppend(RoleMemoryAppendParams {
            caller_pane_id,
            role,
            text: args[1..].join(" "),
        }),
    })?)
}

fn handoff_record(role: ManagedRole, args: &[String]) -> std::io::Result<i32> {
    if args.len() != 9 || args[0] != "record" {
        eprintln!("usage: herdr {} handoff record <task> <attempt> <candidate> <tests-comma-list> <output-paths-comma-list> <findings-comma-list-or-none> <cost> <next-owner>", role_command(role));
        return Ok(2);
    }
    let Some(caller_pane_id) = caller_pane() else {
        eprintln!("hand-off recording must run inside a Herdr pane");
        return Ok(2);
    };
    let split = |value: &str| {
        value
            .split(',')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    super::print_response(&super::send_request(&Request {
        id: format!("cli:{}:handoff:record", role_command(role)),
        method: Method::RoleHandoffRecord(RoleHandoffRecordParams {
            caller_pane_id,
            role,
            task_id: args[1].clone(),
            attempt: args[2].clone(),
            candidate: args[3].clone(),
            tests: split(&args[4]),
            output_paths: split(&args[5]),
            // An empty list cannot be told from a forgotten one, so "none" is written out.
            open_findings: if args[6] == "-" {
                vec!["none".to_string()]
            } else {
                split(&args[6])
            },
            cost: args[7].clone(),
            next_owner: args[8..].join(" "),
        }),
    })?)
}

fn role_command(role: ManagedRole) -> &'static str {
    match role {
        ManagedRole::SpaceOverseer => "overseer",
        ManagedRole::SessionOverlord => "overlord",
    }
}

fn print_overlord_help() {
    eprintln!("herdr overlord commands (chief is an alias):");
    eprintln!("  herdr overlord set <pane>       assign the session overlord");
    eprintln!("  herdr overlord release <pane>   release the session overlord");
    eprintln!("  herdr overlord status [--json]  show this caller and current holders");
    eprintln!("  herdr overlord memory append <text>  append to session role memory");
    eprintln!("  herdr overlord handoff record <task> <attempt> <candidate> <tests> <outputs> <findings|none> <cost> <next-owner>  record checked hand-off");
    eprintln!("  herdr overlord message <pane> <text>  send one line to a space overseer");
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
            delivery_key: None,
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
            delivery_key: None,
        }),
    })?)
}

/// Descriptors shared by the CLI's role card and its registered command surface.
const ROLE_COMMANDS: &[(&str, bool, bool)] = &[
    (
        "herdr overseer status [--pane TARGET] [--json] scope: caller and inspected pane",
        false,
        false,
    ),
    (
        "herdr overseer memory append TEXT scope: this space's append-only role memory",
        true,
        false,
    ),
    (
        "herdr overseer handoff record ... scope: check and record a conversation-bound hand-off",
        true,
        false,
    ),
    (
        "herdr overseer escalate [--wake|--fyi] TEXT scope: current session overlord",
        true,
        false,
    ),
    (
        "herdr overseer message TARGET TEXT scope: one named space overseer",
        false,
        true,
    ),
    (
        "herdr overlord status [--pane TARGET] [--json] scope: caller and inspected pane",
        false,
        false,
    ),
    (
        "herdr overlord memory append TEXT scope: session append-only role memory",
        false,
        true,
    ),
    (
        "herdr overlord handoff record ... scope: check and record a conversation-bound hand-off",
        false,
        true,
    ),
    (
        "herdr overlord message TARGET TEXT scope: one named space overseer",
        false,
        true,
    ),
];

pub(crate) fn role_card(is_overseer: bool, is_overlord: bool) -> String {
    let commands = ROLE_COMMANDS
        .iter()
        .filter(|(_, needs_overseer, needs_overlord)| {
            (!needs_overseer || is_overseer) && (!needs_overlord || is_overlord)
        })
        .map(|(command, _, _)| *command)
        .collect::<Vec<_>>();
    if commands.is_empty() {
        "No active role commands. Read only: herdr overseer status [--pane TARGET] [--json].".into()
    } else {
        commands.join("; ")
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    #[test]
    fn role_card_lists_only_commands_for_held_roles_and_both_role_routes_once() {
        let none = role_card(false, false);
        assert!(none.contains("herdr overseer status"));
        assert!(!none.contains("herdr overseer escalate"));
        let both = role_card(true, true);
        assert_eq!(both.matches("herdr overseer message").count(), 1);
        assert!(both.contains("herdr overlord message"));
    }
}

fn print_overseer_help() {
    eprintln!("herdr overseer commands:");
    eprintln!(
        "  herdr overseer escalate [--wake|--fyi] <text>  wake by default; --fyi updates the situation only"
    );
    eprintln!(
        "  herdr overseer message <pane> <text>   the chief sends one line to a space overseer"
    );
    eprintln!("  herdr overseer set <pane>              assign this pane's space overseer");
    eprintln!("  herdr overseer release <pane>          release this pane's space overseer");
    eprintln!("  herdr overseer status                  show this caller and current holders");
    eprintln!("  herdr overseer memory append <text>  append to this space's role memory");
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

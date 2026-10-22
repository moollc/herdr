//! Terminal errors, observed in harness output (fixture provenance is in BUILD-27).
//! Match error rows, never keywords in an ordinary response or prompt.
use super::{agents, Agent};

pub fn stopped_error_reason(agent: Option<Agent>, content: &str) -> Option<&'static str> {
    let agent = agent?;
    match agent {
        Agent::Codex => {
            // A new output/working block supersedes an older error, including
            // the normal idle composer below it.
            let lines: Vec<_> = content.lines().collect();
            let index = lines
                .iter()
                .rposition(|line| line.starts_with(['■', '•', '✗', '✓']))?;
            let first = lines[index].strip_prefix("■ ")?;
            let mut row = first.to_string();
            for line in &lines[index + 1..] {
                // Wrapped error details are indented; composer/footer text is not.
                if !line.starts_with("  ") || line.trim().is_empty() {
                    break;
                }
                row.push(' ');
                row.push_str(line.trim());
            }
            classify(agent, &row)
        }
        Agent::Claude => {
            let lines: Vec<_> = content.lines().collect();
            let index = lines
                .iter()
                .rposition(|line| line.trim_start().starts_with(['⎿', '●', '⏺']))?;
            let last = lines[index].trim_start().strip_prefix('⎿')?.trim_start();
            // Only working chrome newer than this output can clear it.
            // A stale spinner above the final error is not recovery.
            if agents::claude_code::has_working_chrome(&lines[index + 1..].join("\n")) {
                return None;
            }
            classify(agent, last)
        }
        Agent::Cursor => {
            // Ephemeral error-display rows are unbulleted and persist until
            // input; response bullets and input rows are not error rows.
            for line in content.lines().rev().take(16) {
                if line.contains("ctrl+c to stop") || agents::cursor::has_cursor_spinner(line) {
                    return None;
                }
                if let Some(reason) = classify(agent, line.trim_start()) {
                    return Some(reason);
                }
                if line.trim_start().starts_with(['⬡', '⬢', '•', '●', '›']) {
                    break;
                }
            }
            None
        }
        _ => None,
    }
}

fn classify(agent: Agent, line: &str) -> Option<&'static str> {
    match agent {
        Agent::Codex => {
            if line == "Selected model is at capacity. Please try a different model."
                || line == "Flex capacity unavailable."
            {
                Some("model at capacity")
            } else if line.starts_with("You’ve hit your usage limit")
                || line.starts_with("You've hit your usage limit")
                || line.starts_with("Quota exceeded.")
            {
                Some("usage limit")
            } else if line.starts_with("Your access token could not be refreshed") {
                Some("login expired")
            } else if line.starts_with("stream disconnected before completion:")
                || line.starts_with("Error while reading the server response:")
                || line.starts_with("Connection failed:")
                || line.starts_with("exceeded retry limit, last status:")
            {
                Some("server error")
            } else {
                None
            }
        }
        Agent::Claude => {
            if line.starts_with("You've hit your weekly limit · resets ")
                || line.starts_with("You've hit your limit")
            {
                Some("usage limit")
            } else if line == "Login expired · Please run /login" {
                Some("login expired")
            } else if let Some(error) = line.strip_prefix("API Error: ") {
                if error.starts_with("429 ") {
                    Some("usage limit")
                } else if error.starts_with("401 ") {
                    Some("login expired")
                } else {
                    Some("server error")
                }
            } else {
                None
            }
        }
        Agent::Cursor => {
            if let Some(title) = line.strip_prefix("Error: ") {
                // error-display.ts renders ErrorDetails' server-supplied title
                // in this form. The prefix is structural; keywords only choose
                // the short reason after an error row has been identified.
                let lower = title.to_lowercase();
                if lower.contains("rate limit") || lower.contains("usage limit") {
                    Some("usage limit")
                } else if lower.contains("authentication")
                    || lower.contains("login")
                    || lower.contains("log in")
                {
                    Some("login expired")
                } else if lower.contains("model") && lower.contains("capacity") {
                    Some("model at capacity")
                } else {
                    Some("server error")
                }
            } else if line.starts_with(
                "Authentication failed: your Cursor credentials or API key are invalid or expired.",
            ) || line == "Your stored authentication is invalid. Please log in again."
            {
                Some("login expired")
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::{detect_agent, AgentState};

    #[test]
    fn captured_terminal_errors_block_with_reason() {
        for row in include_str!("fixtures/stopped-errors.tsv")
            .lines()
            .filter(|s| !s.starts_with('#'))
        {
            let cols: Vec<_> = row.split('\t').collect();
            let agent = super::super::parse_agent_label(cols[0]).unwrap();
            let screen = cols[2].replace("\\n", "\n");
            assert_eq!(
                stopped_error_reason(Some(agent), &screen),
                Some(cols[1]),
                "{row}"
            );
            let detection = detect_agent(Some(agent), &screen);
            assert_eq!(detection.state, AgentState::Blocked, "{row}");
            assert!(detection.visible_blocker && !detection.ambiguous, "{row}");
        }
    }

    #[test]
    fn final_error_wins_over_older_working_output() {
        for (agent, screen) in [
            (
                Agent::Claude,
                "✻ Thinking… (esc to interrupt)\n⎿ Login expired · Please run /login",
            ),
            (Agent::Cursor, "⬡ Thinking…\nError: Connection failed"),
        ] {
            assert_eq!(detect_agent(Some(agent), screen).state, AgentState::Blocked);
        }
    }

    #[test]
    fn ordinary_replies_and_prompts_do_not_block() {
        for (agent, screen) in [
            (
                Agent::Codex,
                "• The model is at capacity; usage limit and login expired are examples.\n› ",
            ),
            (
                Agent::Codex,
                "› ■ Selected model is at capacity. Please try a different model.",
            ),
            (
                Agent::Claude,
                "● API Error: is the prefix used for server errors.\n────────\n❯ \n────────",
            ),
            (
                Agent::Cursor,
                "⬢ Error: Connection failed means a network problem.\n→ Add a follow-up",
            ),
            (
                Agent::Cursor,
                "The reply mentions Error: Connection failed and usage limit.",
            ),
        ] {
            assert_eq!(stopped_error_reason(Some(agent), screen), None);
            assert_ne!(detect_agent(Some(agent), screen).state, AgentState::Blocked);
        }
    }

    #[test]
    fn fresh_activity_and_reply_supersede_old_error() {
        for (agent, screen) in [
            (
                Agent::Codex,
                "■ Selected model is at capacity. Please try a different model.\n• Working (2s • esc to interrupt)\n› ",
            ),
            (
                Agent::Codex,
                "■ Selected model is at capacity. Please try a different model.\n• Fixed and complete.\n› ",
            ),
            (
                Agent::Claude,
                "⎿ Login expired · Please run /login\n✻ Thinking… (esc to interrupt)",
            ),
            (
                Agent::Claude,
                "⎿ Login expired · Please run /login\n● Complete.\n────────\n❯ \n────────",
            ),
            (
                Agent::Cursor,
                "Error: Connection failed\n⬡ Thinking…\nctrl+c to stop",
            ),
            (Agent::Cursor, "Error: Connection failed\n⬢ Finished."),
        ] {
            assert_eq!(stopped_error_reason(Some(agent), screen), None, "{screen}");
            assert_ne!(
                detect_agent(Some(agent), screen).state,
                AgentState::Blocked,
                "{screen}"
            );
        }
    }
}

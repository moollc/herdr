//! Once-only policy and a task envelope. Terminal transport owns admission;
//! this state does not claim acknowledgement by the model.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrientationState {
    #[default]
    Suppressed,
    Pending,
    /// A startup hook has received an offer but has not acknowledged emitting it.
    Offered,
    Delivered,
}

impl OrientationState {
    pub fn needs_orientation(self, task: &str) -> bool {
        self == Self::Pending && !task.trim().is_empty()
    }

    pub fn restored(self, live_import: bool) -> Self {
        if live_import {
            self
        } else {
            Self::Suppressed
        }
    }
}

/// The server's durable side of a hook offer.  The nonce binds acknowledgement
/// to one terminal generation and native conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrientationOffer {
    pub session_id: String,
    pub nonce: String,
    pub context: String,
}

pub const OPEN: &str = "[Herdr orientation v1]\n";
pub const TASK: &str = "[/Herdr orientation]\n[User task]\n";

/// Only Auto's verified routing command supports an orientation argument.
/// Other slash commands may interpret arguments themselves; fail explicitly
/// rather than silently changing their semantics on the first task.
pub fn supported_task(task: &str) -> bool {
    !task.starts_with('/') || task == "/who" || task.starts_with("/who ")
}

/// Metadata must stay on a single line and cannot manufacture task delimiters.
/// Bound each value, never the user's task.
pub fn quoted(value: &str) -> String {
    let bounded = if value.chars().count() > 512 {
        "unknown (value exceeds metadata budget)"
    } else {
        value
    };
    let escaped = format!("{:?}", bounded)
        .replace('[', "\\u005b")
        .replace(']', "\\u005d");
    if escaped.len() > 512 {
        "\"unknown (escaped value exceeds metadata budget)\"".into()
    } else {
        escaped
    }
}

pub fn envelope(context: &str, task: &str) -> String {
    if task.trim().is_empty() {
        return task.to_string();
    }
    // Keep slash-command dispatch at the front, including Auto's /who.
    let command = if task.starts_with('/') {
        match task.find(char::is_whitespace) {
            Some(end) => &task[..end],
            None => task,
        }
    } else {
        ""
    };
    if command.is_empty() {
        format!("{OPEN}{context}\n{TASK}{task}")
    } else {
        // The entire original task remains an exact substring in User task.
        // The slash command receives orientation as its argument.
        format!("{command} {OPEN}{context}\n{TASK}{task}")
    }
}

pub fn user_task(text: &str) -> &str {
    if text.starts_with(OPEN)
        || text
            .strip_prefix("/who ")
            .is_some_and(|argument| argument.starts_with(OPEN))
    {
        if let Some((_, task)) = text.split_once(TASK) {
            return task;
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_harness_adapters_preserve_the_enveloped_task() {
        for harness in crate::harness::ALL
            .iter()
            .filter(|harness| harness.agent.is_some())
        {
            let task = harness.message("task with 'quotes'\nand --flags");
            let text = envelope("context", &task);
            let mut adapter = *harness;
            adapter.prefix = "";
            let crate::harness::Launch::Agent { argv, .. } = adapter.launch(&text, false) else {
                panic!("agent adapter");
            };
            assert_eq!(argv.last(), Some(&text));
            assert_eq!(argv.iter().filter(|arg| arg.contains(OPEN)).count(), 1);
            assert_eq!(
                adapter.launch("", false),
                crate::harness::Launch::Agent {
                    agent: adapter.agent.unwrap(),
                    argv: vec![adapter.program().unwrap().into()]
                }
            );
        }
    }

    #[test]
    fn metadata_budget_never_truncates_the_task() {
        let task = "task\n".repeat(5000);
        let context = format!("identity\nself\ncwd\n{}\nlive help", "paths".repeat(3000));
        let text = envelope(&context, &task);
        assert_eq!(user_task(&text), task);
        assert!(text.contains(&context));
        assert!(quoted(&"\n".repeat(500)).contains("unknown"));
    }

    #[test]
    fn first_task_second_prompt_and_blank() {
        let mut state = OrientationState::Pending;
        assert!(!state.needs_orientation(" \n"));
        assert!(state.needs_orientation("task"));
        state = OrientationState::Delivered;
        assert!(!state.needs_orientation("second task"));
    }
    #[test]
    fn resumed_and_legacy_sessions_are_suppressed() {
        assert!(!OrientationState::default().needs_orientation("task"));
        assert!(!OrientationState::Pending
            .restored(false)
            .needs_orientation("task"));
        assert!(OrientationState::Pending
            .restored(true)
            .needs_orientation("task"));
        assert_eq!(
            OrientationState::Delivered.restored(true),
            OrientationState::Delivered
        );
    }
    #[test]
    fn preserves_tasks_and_slash_dispatch() {
        for task in ["fix\n  tests\n", "/who fix tests", "[User task]\nhi"] {
            let result = envelope("context", task);
            assert!(result.ends_with(task));
            assert_eq!(user_task(&result), task);
            if task.starts_with('/') {
                assert!(result.starts_with(task.split_whitespace().next().unwrap()));
            }
        }
        assert_eq!(envelope("context", ""), "");
        assert!(!supported_task("/review"));
        let incidental = format!("/review notes {OPEN}context\n{TASK}literal");
        assert_eq!(user_task(&incidental), incidental);
        assert!(!quoted("\n[User task]").contains('\n'));
        assert!(!quoted("[User task]").contains("[User task]"));
    }
}

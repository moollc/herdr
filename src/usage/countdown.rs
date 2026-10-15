//! The countdown text beside a meter, and when it next changes.

const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;
const WEEK: u64 = 7 * DAY;
const MONTH: u64 = 30 * DAY;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CountdownLabel {
    pub text: String,
    /// Seconds until the text changes. `None` when it never will on its own.
    pub next_change_secs: Option<u64>,
}

/// `1/mo` at 30 days or more, `1/w` at 7 days or more, then `5/d`, `4/h`,
/// `59/m`, `59/s`, whole units rounded down. `idle` once `resets_at` is past,
/// `?` when it is not known.
///
/// `next_change_secs` counts to the moment the number steps down, or to the
/// reset itself for the last second, so a redraw at that time is never early.
pub fn countdown(resets_at: Option<u64>, now: u64) -> CountdownLabel {
    let Some(resets_at) = resets_at else {
        return CountdownLabel {
            text: "?".to_string(),
            next_change_secs: None,
        };
    };
    if resets_at <= now {
        return CountdownLabel {
            text: "idle".to_string(),
            next_change_secs: None,
        };
    }
    let left = resets_at - now;
    let (value, unit_secs, suffix) = if left >= MONTH {
        (left / MONTH, MONTH, "mo")
    } else if left >= WEEK {
        (left / WEEK, WEEK, "w")
    } else if left >= DAY {
        (left / DAY, DAY, "d")
    } else if left >= HOUR {
        (left / HOUR, HOUR, "h")
    } else if left >= MINUTE {
        (left / MINUTE, MINUTE, "m")
    } else {
        (left, 1, "s")
    };
    // The text changes when `left` falls below `value * unit_secs`, which is
    // after `left - (value * unit_secs - 1)` seconds. A unit's last step
    // changes to the next smaller unit at the same boundary.
    let next = left - (value * unit_secs - 1);
    CountdownLabel {
        text: format!("{value}/{suffix}"),
        next_change_secs: Some(next),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(left: u64) -> CountdownLabel {
        countdown(Some(1_000_000 + left), 1_000_000)
    }

    #[test]
    fn units_at_every_boundary() {
        assert_eq!(at(30 * DAY).text, "1/mo");
        assert_eq!(at(30 * DAY - 1).text, "4/w");
        assert_eq!(at(7 * DAY).text, "1/w");
        assert_eq!(at(7 * DAY - 1).text, "6/d");
        assert_eq!(at(DAY).text, "1/d");
        assert_eq!(at(DAY - 1).text, "23/h");
        assert_eq!(at(HOUR).text, "1/h");
        assert_eq!(at(HOUR - 1).text, "59/m");
        assert_eq!(at(MINUTE).text, "1/m");
        assert_eq!(at(MINUTE - 1).text, "59/s");
        assert_eq!(at(1).text, "1/s");
    }

    #[test]
    fn past_none_and_now_have_no_next_change() {
        assert_eq!(at(0).text, "idle");
        assert_eq!(countdown(Some(10), 1_000_000).text, "idle");
        assert_eq!(countdown(Some(10), 1_000_000).next_change_secs, None);
        let unknown = countdown(None, 1_000_000);
        assert_eq!(unknown.text, "?");
        assert_eq!(unknown.next_change_secs, None);
    }

    #[test]
    fn next_change_is_when_the_text_steps() {
        // 4/h holds until three hours are left: one second short of 4 hours
        // never counts, so exactly 4h is 4/h and 1s later is 3/h.
        assert_eq!(at(4 * HOUR).text, "4/h");
        assert_eq!(at(4 * HOUR).next_change_secs, Some(1));
        assert_eq!(at(4 * HOUR + 1800).next_change_secs, Some(1800 + 1));
        assert_eq!(at(3 * HOUR + 1).next_change_secs, Some(2));
        // The step lands on the new text.
        let step = at(4 * HOUR).next_change_secs.unwrap();
        assert_eq!(at(4 * HOUR - step).text, "3/h");
        let step = at(4 * HOUR + 1800).next_change_secs.unwrap();
        assert_eq!(at(4 * HOUR + 1800 - step).text, "3/h");
        // One second before the step the text is unchanged.
        assert_eq!(at(4 * HOUR + 1800 - step + 1).text, "4/h");
    }

    #[test]
    fn seconds_change_every_second_and_end_at_idle() {
        assert_eq!(at(30).next_change_secs, Some(1));
        assert_eq!(at(1).next_change_secs, Some(1));
        assert_eq!(at(1).text, "1/s");
        assert_eq!(at(0).text, "idle");
        // 1/m steps to 59/s after one second.
        assert_eq!(at(MINUTE).next_change_secs, Some(1));
    }

    #[test]
    fn every_step_matches_a_scan() {
        // For a spread of remaining times, the predicted step is the first
        // second where the text differs.
        for left in [
            1, 2, 59, 60, 61, 119, 120, 3599, 3600, 3601, 7200, 86_399, 86_400, 90_000, 604_799,
            604_800, 700_000, 2_591_999, 2_592_000, 5_000_000,
        ] {
            let here = at(left);
            let step = here.next_change_secs.unwrap();
            assert!(step >= 1 && step <= left, "left {left} step {step}");
            assert_eq!(at(left - (step - 1)).text, here.text, "left {left}");
            assert_ne!(at(left - step).text, here.text, "left {left}");
        }
    }
}

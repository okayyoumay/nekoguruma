//! What a job's reported state does to the job's row (design 5.6, ADR-481): the pure rules.
//!
//! [`Report::of`] reads what a `jobState` event says and cleans the fields that reach a column;
//! [`decide`] turns the row's current state and a report into the one write [`crate::agent_events`]
//! runs. The server keeps no copy of the 5.6 transition table: the agent's outbox order is the
//! order, the last report wins while the job is open, the first end is final, and an agent's
//! needsOnSiteRecovery is the only end a later report may replace.
//!
//! Never logged: the detail, which is vehicle data (design 4.3).

use shared_proto::{FailureCode, JobState, MAX_PROCEDURE_CODE_LEN};

/// The longest detail kept, in bytes.
pub(crate) const MAX_DETAIL_LEN: usize = 1024;

/// What a `jobState` event reports, as far as the row takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Report {
    /// The reported state; `None` when its name is not a 5.6 state.
    pub(crate) state: Option<JobState>,
    /// The failure code's name, only for a failed or preconditionFailed report.
    pub(crate) failure_code: Option<&'static str>,
    /// The procedure's own code, only for a failed report with the failure code `procedure`.
    pub(crate) procedure_code: Option<String>,
    /// The detail, only for a failed, preconditionFailed or needsOnSiteRecovery report.
    pub(crate) detail: Option<String>,
}

impl Report {
    /// The report of an event's fields. A failure code the agent may not send (one of the
    /// server's own), an unknown one or none at all is `other`; a procedure code that is not 1 to
    /// [`MAX_PROCEDURE_CODE_LEN`] bytes without U+0000, or that comes with another code, is left
    /// out and never cut; a detail is cleaned by [`clean_detail`].
    pub(crate) fn of(
        state: &str,
        failure_code: Option<&str>,
        procedure_code: Option<&str>,
        detail: Option<&str>,
    ) -> Report {
        let state = JobState::from_wire_name(state);
        let name = state.as_ref().map(JobState::wire_name);
        let with_code = matches!(name, Some("failed" | "preconditionFailed"));
        let with_detail = with_code || name == Some("needsOnSiteRecovery");
        let code = with_code.then(|| agent_failure_code(failure_code));
        let procedure_code = (name == Some("failed") && code == Some("procedure"))
            .then_some(procedure_code)
            .flatten()
            .filter(|p| (1..=MAX_PROCEDURE_CODE_LEN).contains(&p.len()) && !p.contains('\0'))
            .map(str::to_owned);
        let detail = if with_detail {
            detail.and_then(clean_detail)
        } else {
            None
        };
        Report {
            state,
            failure_code: code,
            procedure_code,
            detail,
        }
    }

    /// The state's name for a log line, or `unknown` for one that is not a state. Never the text
    /// the agent sent.
    pub(crate) fn name(&self) -> &'static str {
        self.state.as_ref().map_or("unknown", JobState::wire_name)
    }

    fn terminal(&self) -> bool {
        self.state.as_ref().is_some_and(JobState::is_terminal)
    }
}

/// The name of the failure code the agent reported, kept when it is one an agent may report; the
/// server's own two (an agent's refusal of a command and a retired instruction key, ADR-476), an
/// unknown name and no name are `other`.
fn agent_failure_code(name: Option<&str>) -> &'static str {
    match name.and_then(FailureCode::from_wire_name) {
        Some(FailureCode::InstructionRejected | FailureCode::InstructionKeyRetired) | None => {
            FailureCode::Other.wire_name()
        }
        Some(code) => code.wire_name(),
    }
}

/// The detail as the column keeps it: U+0000, which `text` refuses (the transaction would then
/// fail for good), becomes U+FFFD, the text is cut to [`MAX_DETAIL_LEN`] bytes on a character
/// boundary, and an empty detail is none.
pub(crate) fn clean_detail(detail: &str) -> Option<String> {
    let mut text = detail.replace('\0', "\u{fffd}");
    if text.len() > MAX_DETAIL_LEN {
        let mut end = MAX_DETAIL_LEN;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    (!text.is_empty()).then_some(text)
}

/// The row's state as the decision needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Current {
    /// The `state` column.
    pub(crate) state: String,
    /// Whether `finished_at` is set.
    pub(crate) finished: bool,
}

/// The one write a report leads to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    /// The agent has no such job of the tenant: nothing is written; the caller logs it.
    NoJob,
    /// Not applied; `warn` says whether the caller logs it (a late `received` is routine).
    Ignore { warn: bool },
    /// `received`: the stamps only, never the state.
    Delivered,
    /// The reported state is the row's: the report time only.
    Touch,
    /// An open job moves to another non-terminal state, or an agent's needsOnSiteRecovery job
    /// reopens: the state, the stamps, and (for the reopened job) no end and no failure.
    Move,
    /// The job ends: the state, the failure columns, the end and the stamps; the job's own
    /// vehicle lock is deleted in the same statement.
    End,
}

/// The write for `report` on a job in `current` (none: the agent has no such job).
pub(crate) fn decide(current: Option<&Current>, report: &Report) -> Decision {
    let Some(current) = current else {
        return Decision::NoJob;
    };
    let Some(state) = &report.state else {
        return Decision::Ignore { warn: true };
    };
    let received = *state == JobState::Received;
    // An end that a later report may replace; every other end is final.
    let reopenable = current.finished && current.state == "needsOnSiteRecovery";
    if current.finished && !reopenable {
        return Decision::Ignore { warn: !received };
    }
    if reopenable && received {
        return Decision::Ignore { warn: false };
    }
    if received {
        return Decision::Delivered;
    }
    if report.terminal() {
        return Decision::End;
    }
    if !current.finished && current.state == state.wire_name() {
        return Decision::Touch;
    }
    Decision::Move
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_STATES: [&str; 13] = [
        "received",
        "precheck",
        "ready",
        "running",
        "verifying",
        "awaitingHmi",
        "cancelRequested",
        "completed",
        "failed",
        "cancelled",
        "expired",
        "preconditionFailed",
        "needsOnSiteRecovery",
    ];

    fn report(state: &str) -> Report {
        Report::of(state, None, None, None)
    }

    fn open(state: &str) -> Current {
        Current {
            state: state.to_owned(),
            finished: false,
        }
    }

    fn finished(state: &str) -> Current {
        Current {
            state: state.to_owned(),
            finished: true,
        }
    }

    // J1
    #[test]
    fn a_report_names_its_state_and_an_unknown_name_is_none() {
        for name in ALL_STATES {
            assert_eq!(report(name).name(), name);
        }
        let unknown = report("exploded");
        assert_eq!(unknown.state, None);
        assert_eq!(unknown.name(), "unknown");
    }

    // J2
    #[test]
    fn a_failure_code_is_kept_only_for_failed_and_precondition_failed_and_only_an_agents_own() {
        let code = |state, code| Report::of(state, Some(code), None, None).failure_code;
        assert_eq!(code("failed", "vinMismatch"), Some("vinMismatch"));
        assert_eq!(code("preconditionFailed", "jobRefused"), Some("jobRefused"));
        // O12: the operator refusal's code is an agent's own and is kept, with no detail.
        assert_eq!(
            code("preconditionFailed", "operatorNotAllowed"),
            Some("operatorNotAllowed")
        );
        assert_eq!(
            Report::of("preconditionFailed", Some("operatorNotAllowed"), None, None).detail,
            None
        );
        assert_eq!(code("failed", "procedure"), Some("procedure"));
        // Unknown, wrongly cased and the server's own two are `other`.
        for odd in [
            "nope",
            "VinMismatch",
            "",
            "instructionRejected",
            "instructionKeyRetired",
        ] {
            assert_eq!(code("failed", odd), Some("other"), "{odd:?}");
        }
        assert_eq!(report("failed").failure_code, Some("other"));
        assert_eq!(report("preconditionFailed").failure_code, Some("other"));
        // No code on any other state, whatever the event says.
        for state in ALL_STATES {
            if state != "failed" && state != "preconditionFailed" {
                assert_eq!(code(state, "vinMismatch"), None, "{state}");
            }
        }
    }

    // J2
    #[test]
    fn a_procedure_code_is_kept_only_with_the_code_procedure_and_never_cut() {
        let of = |code: &str, procedure: &str| {
            Report::of("failed", Some(code), Some(procedure), None).procedure_code
        };
        assert_eq!(of("procedure", "P-17"), Some("P-17".to_owned()));
        let longest = "p".repeat(MAX_PROCEDURE_CODE_LEN);
        assert_eq!(of("procedure", &longest), Some(longest.clone()));
        assert_eq!(of("procedure", &format!("{longest}p")), None);
        assert_eq!(of("procedure", ""), None);
        assert_eq!(of("procedure", "a\0b"), None);
        // Bytes, not characters: 43 three-byte characters are 129 bytes.
        assert_eq!(of("procedure", &"\u{3042}".repeat(43)), None);
        assert!(of("procedure", &"\u{3042}".repeat(42)).is_some());
        assert_eq!(of("vinMismatch", "P-17"), None);
        assert_eq!(of("nope", "P-17"), None);
        let no_code = Report::of("failed", None, Some("P-17"), None);
        assert_eq!(no_code.procedure_code, None);
        let precondition = Report::of("preconditionFailed", Some("procedure"), Some("P"), None);
        assert_eq!(precondition.procedure_code, None);
        let completed = Report::of("completed", Some("procedure"), Some("P"), None);
        assert_eq!(completed.procedure_code, None);
    }

    // J3
    #[test]
    fn a_detail_is_cleaned_and_kept_only_for_the_three_states_that_carry_one() {
        assert_eq!(clean_detail(""), None);
        assert_eq!(clean_detail("plain"), Some("plain".to_owned()));
        // U+0000 would make the column refuse the row.
        assert_eq!(clean_detail("a\0b"), Some("a\u{fffd}b".to_owned()));
        assert_eq!(clean_detail("\0"), Some("\u{fffd}".to_owned()));
        let long = clean_detail(&"d".repeat(2000)).unwrap();
        assert_eq!(long.len(), MAX_DETAIL_LEN);
        // A cut never splits a character: 1024 = 341 * 3 + 1, so the cut lands inside one.
        let wide = clean_detail(&"\u{3042}".repeat(500)).unwrap();
        assert_eq!(wide.len(), 341 * 3);
        assert!(wide.chars().all(|c| c == '\u{3042}'));
        // The replacement is counted: 400 U+0000 become 1200 bytes, cut to 341 characters.
        assert_eq!(clean_detail(&"\0".repeat(400)).unwrap().len(), 341 * 3);
        for state in ["failed", "preconditionFailed", "needsOnSiteRecovery"] {
            let r = Report::of(state, None, None, Some("why"));
            assert_eq!(r.detail.as_deref(), Some("why"), "{state}");
            assert_eq!(Report::of(state, None, None, Some("")).detail, None);
        }
        for state in ALL_STATES {
            if !["failed", "preconditionFailed", "needsOnSiteRecovery"].contains(&state) {
                assert_eq!(Report::of(state, None, None, Some("why")).detail, None);
            }
        }
    }

    // J4
    #[test]
    fn the_decision_table_over_every_current_state_and_every_report() {
        let terminal = [
            "completed",
            "failed",
            "cancelled",
            "expired",
            "preconditionFailed",
            "needsOnSiteRecovery",
        ];
        assert_eq!(decide(None, &report("running")), Decision::NoJob);
        for reported in ALL_STATES {
            let r = report(reported);
            // An unknown state name changes nothing and is logged.
            assert_eq!(
                decide(Some(&open("running")), &report("exploded")),
                Decision::Ignore { warn: true }
            );
            for current in ALL_STATES {
                let open_decision = decide(Some(&open(current)), &r);
                let expected = if reported == "received" {
                    Decision::Delivered
                } else if terminal.contains(&reported) {
                    Decision::End
                } else if current == reported {
                    Decision::Touch
                } else {
                    Decision::Move
                };
                assert_eq!(open_decision, expected, "open {current} <- {reported}");
                let ended = decide(Some(&finished(current)), &r);
                let expected = if current == "needsOnSiteRecovery" {
                    if reported == "received" {
                        Decision::Ignore { warn: false }
                    } else if terminal.contains(&reported) {
                        Decision::End
                    } else {
                        Decision::Move
                    }
                } else {
                    Decision::Ignore {
                        warn: reported != "received",
                    }
                };
                assert_eq!(ended, expected, "finished {current} <- {reported}");
            }
        }
    }
}

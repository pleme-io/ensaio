//! A dispatched pipeline run, read as typed progress.
//!
//! Pure: every function takes what `gh` printed and decides. The polling and
//! the cancel live in [`super::env`].

use serde_json::Value;

use super::spec::Await;

/// Where a dispatched run stands relative to what the trial waits for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// Not there yet; poll again.
    Pending,
    /// The awaited job/step completed successfully (or the run completed,
    /// when nothing narrower was awaited).
    Reached,
    /// A job or step failed that the spec did not accept, or the run finished
    /// without ever reaching the awaited point.
    Failed(String),
}

/// The run id in `gh workflow run`'s output: the last path segment of
/// `…/actions/runs/<id>`. `None` when the output carries no run URL — then
/// the run cannot be awaited or stopped, and the dispatch is refused.
#[must_use]
pub fn run_id(dispatch_stdout: &str) -> Option<String> {
    dispatch_stdout
        .split_whitespace()
        .filter_map(|w| w.split("/actions/runs/").nth(1))
        .map(|rest| rest.trim_end_matches('/').to_string())
        .find(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()))
}

fn named(v: &Value) -> &str {
    v.get("name").and_then(Value::as_str).unwrap_or_default()
}

fn field<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    hay.to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

fn failed(conclusion: &str) -> bool {
    matches!(
        conclusion,
        "failure" | "timed_out" | "cancelled" | "startup_failure" | "action_required"
    )
}

/// Whether the run is still going (anything but `completed`).
#[must_use]
pub fn in_flight(view: &Value) -> bool {
    field(view, "status") != "completed"
}

/// Judge one `gh run view --json status,conclusion,jobs` answer.
///
/// An unaccepted failure is checked FIRST, so a run that reached the awaited
/// step and then failed a later, unaccepted step is still reported failed on
/// the next poll rather than as placed.
#[must_use]
pub fn progress(view: &Value, until: Option<&Await>, accept: &[String]) -> Progress {
    let accepted = |name: &str| accept.iter().any(|a| contains_ci(name, a));
    let jobs: Vec<&Value> = view
        .get("jobs")
        .and_then(Value::as_array)
        .map(|j| j.iter().collect())
        .unwrap_or_default();

    // A failing step fails the run unless it, or its job, is accepted. A
    // failed job with no failing step on record (a job that never started its
    // steps) fails the run unless the job is accepted.
    for job in &jobs {
        if accepted(named(job)) {
            continue;
        }
        let steps: Vec<&Value> = job
            .get("steps")
            .and_then(Value::as_array)
            .map(|s| s.iter().collect())
            .unwrap_or_default();
        let failing: Vec<&&Value> = steps
            .iter()
            .filter(|s| failed(field(s, "conclusion")))
            .collect();
        if let Some(s) = failing.iter().find(|s| !accepted(named(s))) {
            return Progress::Failed(format!(
                "step `{}` in job `{}` concluded {}",
                named(s),
                named(job),
                field(s, "conclusion")
            ));
        }
        if failing.is_empty() && failed(field(job, "conclusion")) {
            return Progress::Failed(format!(
                "job `{}` concluded {}",
                named(job),
                field(job, "conclusion")
            ));
        }
    }

    let reached = match until {
        None => !in_flight(view),
        Some(a) => jobs
            .iter()
            .filter(|j| contains_ci(named(j), &a.job))
            .any(|j| match &a.step {
                None => {
                    field(j, "status") == "completed"
                        && (field(j, "conclusion") == "success" || accepted(named(j)))
                }
                Some(step) => j.get("steps").and_then(Value::as_array).is_some_and(|ss| {
                    ss.iter().any(|s| {
                        contains_ci(named(s), step)
                            && field(s, "status") == "completed"
                            && field(s, "conclusion") == "success"
                    })
                }),
            }),
    };
    if reached {
        return Progress::Reached;
    }
    if !in_flight(view) {
        let what = until.map_or_else(
            || "completion".to_string(),
            |a| match &a.step {
                Some(s) => format!("job `{}` step `{s}`", a.job),
                None => format!("job `{}`", a.job),
            },
        );
        return Progress::Failed(format!(
            "the run completed ({}) without reaching {what}",
            field(view, "conclusion")
        ));
    }
    Progress::Pending
}

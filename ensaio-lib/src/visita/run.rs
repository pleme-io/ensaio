//! The visita interpreter: capture, steps, undo, prove.
//!
//! 1. **Capture** the natural state. Nothing is written before this succeeds.
//! 2. **Steps**, in declared order. A placement that fails stops the steps —
//!    judging a half-changed environment produces a confident verdict about
//!    something nobody declared — but never stops the undo.
//! 3. **Undo**, always: every fixture actually placed, in reverse order, by its
//!    declared cleanup. A failed undo does not stop the others. A fixture
//!    declared `:undo-phase after-restore` waits until the restore has
//!    settled, and the diff is taken again after it.
//! 4. **Prove**: re-capture and diff against the natural state, re-trying
//!    until empty or `:settle` expires. The result is the
//!    `natural-state-restored` assertion, judged like any other: the checked
//!    objects are its subjects, every missing / extra / differing object a
//!    finding. So "restored" over zero objects is `Vacuous`, never a pass.

use std::time::{Duration, Instant};

use serde::Serialize;

use super::command::now;
use super::env::OperatorEnvironment;
use super::snapshot::{self, NaturalState, RestoreDiff};
use super::spec::{Step, VisitaSpec};
use crate::fixture::{Cleanup, Fixture};
use crate::interp::EnsaioEnvironment;
use crate::receipt::{CertificationInput, CertificationReceipt};
use crate::types::{AssertionKind, AssertionResult, Observation, Observations, Verdict};

/// The assertions whose record starts at the placement at step `idx`: those
/// with `:record-from` naming that fixture whose next observe comes before the
/// fixture is placed again.
#[must_use]
pub fn records_starting_at(spec: &VisitaSpec, idx: usize) -> Vec<crate::types::AssertionSpec> {
    let Some(Step::Place(fixture)) = spec.steps.get(idx) else {
        return Vec::new();
    };
    let later = &spec.steps[idx + 1..];
    let next_place = later
        .iter()
        .position(|s| matches!(s, Step::Place(n) if n == fixture))
        .unwrap_or(later.len());
    spec.assertions
        .iter()
        .filter(|a| a.operator.record_from.as_deref() == Some(fixture.as_str()))
        .filter(|a| {
            later
                .iter()
                .position(|s| matches!(s, Step::Observe(id) if id == &a.id))
                .is_some_and(|obs| obs < next_place)
        })
        .cloned()
        .collect()
}

/// The id the restore proof is recorded under.
pub const RESTORED_ID: &str = "natural-state-restored";

/// The id the guard's verdict is recorded under.
pub const GUARD_ID: &str = "guard";

/// The id "every declared step ran" is recorded under.
pub const STEPS_ID: &str = "steps-completed";

#[derive(Serialize, Debug, Clone)]
pub struct StepRecord {
    pub at: String,
    pub step: String,
    pub outcome: String,
}

#[derive(Serialize, Debug, Clone)]
pub struct VisitaReport {
    pub name: String,
    pub claim: String,
    pub started_at: String,
    pub finished_at: String,
    pub natural_state_address: String,
    pub steps: Vec<StepRecord>,
    pub placed: Vec<String>,
    pub undone: Vec<String>,
    pub undo_errors: Vec<String>,
    pub aborted: Option<String>,
    pub guard_trip: Option<super::guard::GuardTrip>,
    pub restore_attempts: usize,
    pub restore: RestoreDiff,
    pub results: Vec<AssertionResult>,
    pub receipt: CertificationReceipt,
}

impl VisitaReport {
    /// Restored AND every observation held.
    #[must_use]
    pub fn passed(&self) -> bool {
        matches!(self.receipt.verdict, crate::receipt::CertVerdict::Certified)
    }
}

/// Run one visita.
///
/// # Errors
///
/// Only when the natural state could not be captured — the one failure after
/// which nothing has been changed and nothing may be. Every later failure is
/// recorded in the report, because by then the environment has been touched
/// and the report is the only honest account of it.
pub fn run(spec: &VisitaSpec, env: &OperatorEnvironment<'_>) -> Result<VisitaReport, String> {
    let started_at = now();
    let ev = env.operator().evidence();
    let allow = snapshot::effective_allow(&spec.allow);

    // Every branch the trial will deploy must contain its trunk — checked
    // before ANYTHING is read into the snapshot or written to the target.
    for b in &spec.branch_contains {
        env.check_branch("start", b)?;
    }
    let natural = snapshot::capture(env.operator(), &spec.natural_state, "natural-state")?;
    let _ = ev.write(
        "natural-state.json",
        serde_json::to_vec_pretty(&natural)
            .unwrap_or_default()
            .as_slice(),
    );
    env.set_natural(natural.clone());
    // The guard reads its pods before anything changes: a guard that cannot
    // see its subjects refuses the run while refusing is still free.
    if let Some(g) = &spec.guard {
        env.start_guard(g)?;
    }
    env.acquire_lease()?;

    let mut steps = Vec::new();
    let mut placed: Vec<&Fixture> = Vec::new();
    let mut results: Vec<AssertionResult> = Vec::new();
    let mut aborted = None;
    let mut undone = Vec::new();
    let mut undo_errors = Vec::new();
    env.track(&placed, false);

    for (idx, step) in spec.steps.iter().enumerate() {
        let (label, outcome) = match step {
            Step::Place(n) => {
                // Records that watch this change start BEFORE it is placed —
                // but only records whose observe comes next for THIS
                // placement: a fixture placed once per phase starts each
                // phase's record at its own phase, never an earlier one.
                env.begin_recording(n, &records_starting_at(spec, idx));
                let f = spec.fixtures.iter().find(|f| &f.name == n);
                // Trunk can move mid-run: a fixture that pins or dispatches a
                // checked branch re-reads it first, and is refused BEFORE its
                // write if the branch has fallen behind.
                let stale = f.and_then(|f| f.action.as_ref()).and_then(|a| {
                    spec.branch_contains
                        .iter()
                        .filter(|b| a.names_branch(&b.branch))
                        .find_map(|b| env.check_branch(&format!("place:{n}"), b).err())
                        .or_else(|| env.check_absorb(&format!("place:{n}"), a).err())
                });
                if let Some(why) = stale {
                    aborted = Some(why.clone());
                    let rec = StepRecord {
                        at: now(),
                        step: format!("place {n}"),
                        outcome: format!("refused: {why}"),
                    };
                    ev.note(&rec);
                    steps.push(rec);
                    break;
                }
                match f.map(|f| (f, env.place_fixture(f))) {
                    Some((f, Ok(()))) => {
                        placed.push(f);
                        (format!("place {n}"), "ok".to_string())
                    }
                    Some((f, Err(e))) => {
                        // A mutating verb may have landed before the failure
                        // was noticed (a fault's eval ran, a patch half-applied
                        // is not a thing, but an annotate that timed out may
                        // have been accepted). Undo it anyway: its inverse is
                        // idempotent against the natural state.
                        placed.push(f);
                        aborted = Some(format!("place {n}: {e}"));
                        (format!("place {n}"), format!("failed: {e}"))
                    }
                    None => {
                        aborted = Some(format!("place {n}: no such fixture"));
                        (format!("place {n}"), "no such fixture".into())
                    }
                }
            }
            Step::Observe(id) => match spec.assertions.iter().find(|a| &a.id == id) {
                Some(a) => {
                    let verdict = env
                        .run_assertion(a)
                        .unwrap_or_else(|message| Verdict::Errored { message });
                    let tag = verdict.tag().to_string();
                    ev.note(&serde_json::json!({"at": now(), "verdict": id, "kind": a.kind, "result": verdict}));
                    results.push(AssertionResult {
                        assertion_id: a.id.clone(),
                        kind: a.kind,
                        verdict,
                    });
                    (format!("observe {id}"), tag)
                }
                None => {
                    aborted = Some(format!("observe {id}: no such assertion"));
                    (format!("observe {id}"), "no such assertion".into())
                }
            },
            Step::Wait(d) => {
                let dur = humantime::parse_duration(d).unwrap_or_default();
                match env.pause(dur) {
                    Ok(()) => (format!("wait {d}"), "ok".into()),
                    Err(why) => {
                        aborted = Some(why.clone());
                        (format!("wait {d}"), why)
                    }
                }
            }
            // A phase's load comes off before the next phase: undo now, by
            // the fixture's declared cleanup, and take it off the stack.
            Step::Undo(n) => match placed.iter().position(|f| &f.name == n) {
                Some(i) => {
                    let f = placed.remove(i);
                    match env.clean_fixture(f) {
                        Ok(()) => {
                            undone.push(f.name.clone());
                            (format!("undo {n}"), "undone".to_string())
                        }
                        Err(e) => {
                            undo_errors.push(format!("{n}: {e}"));
                            aborted = Some(format!("undo {n}: {e}"));
                            (format!("undo {n}"), format!("failed: {e}"))
                        }
                    }
                }
                None => (format!("undo {n}"), "not placed".into()),
            },
            Step::Phase(name) => (format!("phase {name}"), "begin".into()),
        };
        let rec = StepRecord {
            at: now(),
            step: label,
            outcome,
        };
        ev.note(&rec);
        steps.push(rec);
        env.track(&placed, false);
        env.renew_lease();
        if aborted.is_none() {
            aborted = env.guard_tripped();
        }
        if aborted.is_some() {
            break;
        }
    }

    // ── guard tripped: the loaded pods come back FIRST ─────────────────
    //
    // A starved broker is the thing that must never be left behind, so when
    // the guard trips, every fixture placed on the pod that tripped it is
    // undone first, then every other in-place resize — before runs are
    // stopped, before any other inverse.
    let mut undone_early: Vec<String> = Vec::new();
    if let (Some(trip), Some(g)) = (env.guard_trip(), &spec.guard) {
        let on_tripped = |f: &&&Fixture| {
            f.undo_phase.is_in_order() && env.touches_pod(f, &g.context, &g.namespace, &trip.pod)
        };
        let first: Vec<&Fixture> = placed
            .iter()
            .rev()
            .filter(on_tripped)
            .chain(placed.iter().rev().filter(|f| {
                !on_tripped(f)
                    && f.undo_phase.is_in_order()
                    && matches!(f.action, Some(super::spec::Action::ResizePod { .. }))
            }))
            .copied()
            .collect();
        for f in first {
            let outcome = match env.clean_fixture(f) {
                Ok(()) => {
                    undone.push(f.name.clone());
                    format!("undone first (guard tripped on `{}`)", trip.pod)
                }
                Err(e) => {
                    undo_errors.push(format!("{}: {e}", f.name));
                    format!("undo failed: {e}")
                }
            };
            undone_early.push(f.name.clone());
            let rec = StepRecord {
                at: now(),
                step: format!("undo {}", f.name),
                outcome,
            };
            ev.note(&rec);
            steps.push(rec);
        }
    }

    // ── stop: every dispatched run, before anything is put back ────────
    //
    // A pipeline still in flight can sync, pin or validate AFTER the undo and
    // turn a restore into a half-restore (observed: an unpin at 21:16 while two
    // dispatched runs were still going, one of which then synced and failed).
    // So every run is cancelled and awaited FIRST, and if any will not stop,
    // nothing else is undone: the diff then names what is still changed,
    // rather than the undo racing a run it cannot see.
    let in_flight = env.stop_runs();
    let hold_back = !in_flight.is_empty();
    for e in &in_flight {
        undo_errors.push(format!("stop: {e}"));
    }
    if hold_back {
        let rec = StepRecord {
            at: now(),
            step: "stop dispatched runs".into(),
            outcome: format!("held back every undo: {}", in_flight.join("; ")),
        };
        ev.note(&rec);
        steps.push(rec);
    }

    // ── undo: always, reverse of placement ─────────────────────────────
    //
    // Except `:undo-phase after-restore` fixtures: something the restore
    // still needs (a proxy its clients are pointed at) comes off only after
    // the restore has converged, below.
    let deferred: Vec<&Fixture> = placed
        .iter()
        .rev()
        .filter(|f| !undone_early.contains(&f.name) && !f.undo_phase.is_in_order())
        .copied()
        .collect();
    for f in placed
        .iter()
        .rev()
        .filter(|f| !undone_early.contains(&f.name) && f.undo_phase.is_in_order())
    {
        let rec = undo_one(env, f, hold_back, "", &mut undone, &mut undo_errors);
        ev.note(&rec);
        steps.push(rec);
    }

    // ── restore: placed after every undo, before the diff ──────────────
    //
    // Runs even when the steps aborted: a pipeline that was dispatched and
    // then failed still left the app on the branch.
    for n in spec.restore.iter().filter(|_| !hold_back) {
        let outcome = match spec.fixtures.iter().find(|f| &f.name == n) {
            Some(f) => match env.place_fixture(f) {
                Ok(()) => {
                    undone.push(format!("restore:{n}"));
                    "ok".to_string()
                }
                Err(e) => {
                    undo_errors.push(format!("restore {n}: {e}"));
                    format!("failed: {e}")
                }
            },
            None => {
                undo_errors.push(format!("restore {n}: no such fixture"));
                "no such fixture".into()
            }
        };
        let rec = StepRecord {
            at: now(),
            step: format!("restore {n}"),
            outcome,
        };
        ev.note(&rec);
        steps.push(rec);
    }

    // ── prove ──────────────────────────────────────────────────────────
    let settle = spec
        .settle
        .as_deref()
        .and_then(|s| humantime::parse_duration(s).ok())
        .unwrap_or(Duration::ZERO);
    let (mut restore, mut attempts) = settle_diff(
        env,
        spec,
        &natural,
        &allow,
        settle,
        &env.held_objects(&deferred),
    );
    if !deferred.is_empty() {
        let _ = ev.write(
            "restore-diff-held.json",
            serde_json::to_vec_pretty(&restore)
                .unwrap_or_default()
                .as_slice(),
        );
        for f in &deferred {
            let rec = undo_one(
                env,
                f,
                hold_back,
                " (after restore)",
                &mut undone,
                &mut undo_errors,
            );
            ev.note(&rec);
            steps.push(rec);
        }
        let (last, more) = settle_diff(env, spec, &natural, &allow, settle, &[]);
        restore = last;
        attempts += more;
    }
    let still_placed: Vec<&Fixture> = placed
        .iter()
        .filter(|f| !undone.contains(&f.name))
        .copied()
        .collect();
    if let Err(e) = env.release_lease() {
        undo_errors.push(format!("lease: {e}"));
    }
    env.track(&still_placed, true);
    let _ = ev.write(
        "restore-diff.json",
        serde_json::to_vec_pretty(&restore)
            .unwrap_or_default()
            .as_slice(),
    );
    results.push(AssertionResult {
        assertion_id: RESTORED_ID.into(),
        kind: AssertionKind::NaturalStateRestored,
        verdict: restored_verdict(&natural, &restore, &undo_errors),
    });
    // The guard's own verdict. A trip is witnessed evidence against the
    // guard's claim — Falsified, naming the pod, the signal and the time —
    // and as a gating precondition it makes the whole receipt Inconclusive:
    // the claim under test was stopped before it could answer.
    if spec.guard.is_some() {
        let (probes, pods) = env.guard_summary().unwrap_or_default();
        let subjects = Observations::scope(vec![Observation::new(format!(
            "{probes} probes of {}",
            pods.join(", ")
        ))]);
        let verdict = match env.guard_trip() {
            Some(trip) => Verdict::judge(subjects, vec![Observation::new(trip.to_string())]),
            None => Verdict::judge(
                Observations::scope(vec![Observation::new(format!(
                    "{probes} probes of {}: every pod stayed Ready, none restarted, no liveness run near its kill",
                    pods.join(", ")
                ))]),
                Vec::new(),
            ),
        };
        results.push(AssertionResult {
            assertion_id: GUARD_ID.into(),
            kind: AssertionKind::GuardHeld,
            verdict,
        });
    }
    // Did the trial run at all? An aborted one answered nothing — even with a
    // clean restore it must not certify.
    let steps_verdict = match &aborted {
        Some(why) => Verdict::Skipped {
            reason: format!("the trial aborted: {why}"),
        },
        None => Verdict::judge(
            Observations::scope(
                spec.steps
                    .iter()
                    .map(|s| Observation::new(format!("{s:?}")))
                    .collect::<Vec<_>>(),
            ),
            Vec::new(),
        ),
    };
    results.push(AssertionResult {
        assertion_id: STEPS_ID.into(),
        kind: AssertionKind::StepsCompleted,
        verdict: steps_verdict,
    });

    let receipt = CertificationReceipt::build(
        CertificationInput {
            run_id: spec.name.clone(),
            scope: "visita".into(),
            // Declared gates (an A/B's "side A reproduced") plus the guard:
            // any of them not held makes the receipt Inconclusive.
            precondition_ids: spec
                .preconditions
                .iter()
                .cloned()
                .chain(spec.guard.as_ref().map(|_| GUARD_ID.to_string()))
                .chain(std::iter::once(STEPS_ID.to_string()))
                .collect(),
            known_gaps: Vec::new(),
            at: now(),
        },
        &results,
    );

    let report = VisitaReport {
        name: spec.name.clone(),
        claim: spec.claim.clone(),
        started_at,
        finished_at: now(),
        natural_state_address: natural.address.clone(),
        steps,
        placed: placed.iter().map(|f| f.name.clone()).collect(),
        undone,
        undo_errors,
        aborted,
        guard_trip: env.guard_trip(),
        restore_attempts: attempts,
        restore,
        results,
        receipt,
    };
    let _ = ev.write(
        "report.json",
        serde_json::to_vec_pretty(&report)
            .unwrap_or_default()
            .as_slice(),
    );
    let _ = ev.write("summary.md", summary(&report).as_bytes());
    Ok(report)
}

/// Undo one placed fixture by its declared cleanup, held back while a
/// dispatched run is in flight, and say what happened.
fn undo_one(
    env: &OperatorEnvironment<'_>,
    f: &Fixture,
    hold_back: bool,
    when: &str,
    undone: &mut Vec<String>,
    undo_errors: &mut Vec<String>,
) -> StepRecord {
    let step = format!("undo {}{when}", f.name);
    if hold_back && !matches!(f.cleanup, Cleanup::Stop) {
        return StepRecord {
            at: now(),
            step,
            outcome: "held back: a dispatched run is still in flight".into(),
        };
    }
    let needs_action = matches!(
        f.cleanup,
        Cleanup::Inverse(_) | Cleanup::RestoreFromSnapshot | Cleanup::Stop | Cleanup::OwnInverse
    );
    let outcome = match (env.clean_fixture(f), needs_action) {
        (Ok(()), true) => {
            undone.push(f.name.clone());
            "undone".to_string()
        }
        (Ok(()), false) => format!("nothing to undo ({})", cleanup_word(&f.cleanup)),
        (Err(e), _) => {
            undo_errors.push(format!("{}: {e}", f.name));
            format!("undo failed: {e}")
        }
    };
    StepRecord {
        at: now(),
        step,
        outcome,
    }
}

fn cleanup_word(c: &Cleanup) -> &'static str {
    match c {
        Cleanup::EnvironmentIsDestroyed => "environment-is-destroyed",
        Cleanup::Inverse(_) => "inverse",
        Cleanup::RestoreFromSnapshot => "restore-from-snapshot",
        Cleanup::Transient { .. } => "transient",
        Cleanup::Retained { .. } => "retained",
        Cleanup::Stop => "stop",
        Cleanup::OwnInverse => "own-inverse",
    }
}

fn settle_diff(
    env: &OperatorEnvironment<'_>,
    spec: &VisitaSpec,
    natural: &NaturalState,
    allow: &[String],
    settle: Duration,
    held: &[snapshot::ObjectKey],
) -> (RestoreDiff, usize) {
    let deadline = Instant::now() + settle;
    let mut attempts = 0;
    loop {
        attempts += 1;
        let d = match snapshot::capture(env.operator(), &spec.natural_state, "restore-check") {
            Ok(live) => snapshot::diff(natural, &snapshot::without(live, held), allow),
            // A capture that fails after undo — a name that no longer exists
            // is the usual cause — is reported as that name missing, not as an
            // empty diff.
            Err(e) => RestoreDiff {
                checked: natural.objects.len(),
                missing: vec![format!("restore capture failed: {e}")],
                ..RestoreDiff::default()
            },
        };
        if d.is_empty() || Instant::now() >= deadline {
            return (d, attempts);
        }
        std::thread::sleep(Duration::from_secs(10).min(settle));
    }
}

/// The restore proof as a verdict: subjects are the objects checked, findings
/// every object not back to natural, plus every undo that failed.
#[must_use]
pub fn restored_verdict(
    natural: &NaturalState,
    d: &RestoreDiff,
    undo_errors: &[String],
) -> Verdict {
    let subjects: Vec<Observation> = natural
        .objects
        .keys()
        .map(|k| Observation::new(format!("{k} @ {}", &natural.address[..16])))
        .collect();
    let mut findings: Vec<Observation> = Vec::new();
    findings.extend(
        d.missing
            .iter()
            .map(|m| Observation::new(format!("missing: {m}"))),
    );
    findings.extend(
        d.extra
            .iter()
            .map(|m| Observation::new(format!("extra: {m}"))),
    );
    for o in &d.differing {
        for c in &o.changes {
            findings.push(Observation::new(format!(
                "{}: {} natural={} live={}",
                o.object,
                c.path,
                c.natural
                    .as_ref()
                    .map_or("<absent>".into(), ToString::to_string),
                c.live
                    .as_ref()
                    .map_or("<absent>".into(), ToString::to_string)
            )));
        }
    }
    findings.extend(
        undo_errors
            .iter()
            .map(|e| Observation::new(format!("undo failed: {e}"))),
    );
    Verdict::judge(Observations::scope(subjects), findings)
}

fn summary(r: &VisitaReport) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(s, "# visita: {}\n", r.name);
    let _ = writeln!(s, "{}\n", r.claim);
    let _ = writeln!(
        s,
        "- verdict: **{:?}**\n- window: {} .. {}\n- natural state: `{}`\n- restore: {} checked, {} differing, {} missing, {} extra, {} allowed changes, {} attempt(s)\n",
        r.receipt.verdict,
        r.started_at,
        r.finished_at,
        r.natural_state_address,
        r.restore.checked,
        r.restore.differing.len(),
        r.restore.missing.len(),
        r.restore.extra.len(),
        r.restore.allowed,
        r.restore_attempts
    );
    if let Some(a) = &r.aborted {
        let _ = writeln!(s, "- **aborted:** {a}\n");
    }
    if let Some(t) = &r.guard_trip {
        let _ = writeln!(s, "- **guard tripped:** {t}\n");
    }
    let _ = writeln!(s, "## Steps\n\n| at | step | outcome |\n|---|---|---|");
    for st in &r.steps {
        let _ = writeln!(s, "| {} | {} | {} |", st.at, st.step, st.outcome);
    }
    let _ = writeln!(
        s,
        "\n## Assertions\n\n| id | kind | verdict | subjects |\n|---|---|---|---|"
    );
    for res in &r.results {
        let verdict = match (&r.guard_trip, res.kind) {
            (Some(t), AssertionKind::GuardHeld) => {
                format!("{} — tripped by `{}` at {}", res.verdict.tag(), t.pod, t.at)
            }
            _ => res.verdict.tag().to_string(),
        };
        let _ = writeln!(
            s,
            "| {} | {:?} | {verdict} | {} |",
            res.assertion_id,
            res.kind,
            res.verdict.subject_count()
        );
    }
    // A rollout verdict is only as good as what was watched: print, per
    // Deployment, how many samples caught it mid-rollout and when.
    for res in r.results.iter().filter(|x| {
        matches!(
            x.kind,
            AssertionKind::RolloutSurgeBound | AssertionKind::ObjectFields
        )
    }) {
        let _ = writeln!(s, "\n### {} — what was watched\n", res.assertion_id);
        match &res.verdict {
            Verdict::Held { subjects } | Verdict::Falsified { subjects, .. } => {
                for o in subjects.iter() {
                    let _ = writeln!(s, "- {}", o.as_str());
                }
            }
            Verdict::Skipped { reason } => {
                let _ = writeln!(s, "- {reason}");
            }
            Verdict::Errored { .. } | Verdict::Vacuous {} | Verdict::Unreached {} => {
                let _ = writeln!(
                    s,
                    "- {}",
                    if res.kind == AssertionKind::ObjectFields {
                        "the object was never read"
                    } else {
                        "no generation of the expected build was recorded"
                    }
                );
            }
        }
    }
    for res in &r.results {
        if let Verdict::Falsified { findings, .. } = &res.verdict {
            let _ = writeln!(s, "\n### {} findings\n", res.assertion_id);
            for f in findings.iter() {
                let _ = writeln!(s, "- {}", f.as_str());
            }
        }
        if let Verdict::Errored { message } = &res.verdict {
            let _ = writeln!(s, "\n### {} errored\n\n{message}", res.assertion_id);
        }
    }
    let _ = writeln!(
        s,
        "\nRaw output for every command: `steps.jsonl` and `raw/`."
    );
    s
}

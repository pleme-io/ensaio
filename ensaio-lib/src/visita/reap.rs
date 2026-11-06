use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use super::command::now;
use super::env::OperatorEnvironment;
use super::lease::{self, RunPlan};
use super::snapshot::{self, NaturalState, RestoreDiff};
use crate::fixture::Fixture;
use crate::interp::EnsaioEnvironment;
use crate::types::Verdict;

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct Reapable {
    pub lease: String,
    pub run: String,
    pub holder: String,
}

pub fn select(
    leases: &[Value],
    operator: &str,
    run: Option<&str>,
    at: DateTime<Utc>,
) -> Result<Vec<Reapable>, String> {
    let row = |l: &Value| Reapable {
        lease: lease::name_of(l).to_string(),
        run: lease::run_of(l).unwrap_or_default().to_string(),
        holder: lease::holder(l).unwrap_or_default().to_string(),
    };
    if let Some(r) = run {
        let l = leases
            .iter()
            .find(|l| lease::run_of(l) == Some(r))
            .ok_or_else(|| format!("no visita lease for run `{r}`"))?;
        if !lease::held_by(l, operator) {
            return Err(format!(
                "refused: lease `{}` is held by `{}`, not by `{operator}`; a reaper touches only its own operator's leases",
                lease::name_of(l),
                lease::holder(l).unwrap_or("?")
            ));
        }
        return Ok(vec![row(l)]);
    }
    Ok(leases
        .iter()
        .filter(|l| {
            lease::held_by(l, operator) && lease::expired(l, at) && lease::run_of(l).is_some()
        })
        .map(row)
        .collect())
}

#[derive(Serialize, Debug, Clone)]
pub struct ReapReport {
    pub run: String,
    pub at: String,
    pub undone: Vec<String>,
    pub undo_errors: Vec<String>,
    pub restore: RestoreDiff,
    pub verdict: Verdict,
}

pub fn reap(
    env: &OperatorEnvironment<'_>,
    plan: &RunPlan,
    natural: &NaturalState,
) -> Result<ReapReport, String> {
    env.load_ledger(&plan.ledger)?;
    env.set_natural(natural.clone());
    if let Some(l) = &plan.lease {
        env.adopt_lease(l.clone());
    }
    let spec = &plan.spec;
    let placed: Vec<&Fixture> = plan
        .placed
        .iter()
        .filter_map(|n| spec.fixtures.iter().find(|f| &f.name == n))
        .collect();
    let mut undone = Vec::new();
    let mut undo_errors = Vec::new();
    let order = placed
        .iter()
        .rev()
        .filter(|f| f.undo_phase.is_in_order())
        .chain(placed.iter().rev().filter(|f| !f.undo_phase.is_in_order()));
    for f in order {
        match env.clean_fixture(f) {
            Ok(()) => undone.push(f.name.clone()),
            Err(e) => undo_errors.push(format!("{}: {e}", f.name)),
        }
        env.track(
            &placed
                .iter()
                .filter(|p| !undone.contains(&p.name))
                .copied()
                .collect::<Vec<_>>(),
            false,
        );
    }
    for n in &spec.restore {
        if let Some(f) = spec.fixtures.iter().find(|f| &f.name == n) {
            match env.place_fixture(f) {
                Ok(()) => undone.push(format!("restore:{n}")),
                Err(e) => undo_errors.push(format!("restore {n}: {e}")),
            }
        }
    }
    let restore = match snapshot::capture(env.operator(), &spec.natural_state, "reap-check") {
        Ok(live) => snapshot::diff(natural, &live, &snapshot::effective_allow(&spec.allow)),
        Err(e) => RestoreDiff {
            checked: natural.objects.len(),
            missing: vec![format!("restore capture failed: {e}")],
            ..RestoreDiff::default()
        },
    };
    if let Err(e) = env.release_lease() {
        undo_errors.push(format!("lease: {e}"));
    }
    env.track(&[], undo_errors.is_empty());
    let verdict = super::run::restored_verdict(natural, &restore, &undo_errors);
    let report = ReapReport {
        run: plan.run.clone(),
        at: now(),
        undone,
        undo_errors,
        restore,
        verdict,
    };
    let ev = env.operator().evidence();
    let _ = ev.write(
        "restore-diff.json",
        serde_json::to_vec_pretty(&report.restore)
            .unwrap_or_default()
            .as_slice(),
    );
    let _ = ev.write(
        "reap.json",
        serde_json::to_vec_pretty(&report)
            .unwrap_or_default()
            .as_slice(),
    );
    Ok(report)
}

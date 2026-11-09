//! Per-experiment data placement — the layer ensaio owns in the four-layer
//! split.
//!
//! **Base state is SYNCED; experiment data is PLACED.** Controllers drive the
//! config, model and seed every environment must have, identically. A fixture
//! is the opposite: one trial needs it, and two environments legitimately
//! differ because of it. Getting that line wrong loses something quietly in
//! either direction — a base seed placed per-experiment makes environments
//! non-comparable, and an experiment fixture written into the base contaminates
//! every other trial.
//!
//! ## Why this type exists
//!
//! Until now ensaio had **no way to say "place this data"**. `EnsaioSpec` has
//! no data/fixture/seed field and [`crate::interp::EnsaioEnvironment`]'s
//! methods place nothing. The only available path was smuggling mutation
//! through a `CustomJob` assertion's `command` string — and the shipped
//! RabbitMQ certification spec did exactly that, `PUT`ing a policy
//! and a queue and then deleting both, as shell inside a probe.
//!
//! Three things are wrong with that, and they are why this is a type rather
//! than a convention:
//!
//! 1. **A mutation is indistinguishable from a probe.** Both are a
//!    `CustomJob` with a command. Nothing can report what a run CHANGED,
//!    because nothing knows which commands changed anything.
//! 2. **Cleanup is hand-written in the same string** — so a failure between
//!    the `PUT` and the `DELETE` leaves residue no one is tracking, in an
//!    environment the next trial may reuse.
//! 3. **It cannot be ordered against the base.** A fixture that needs the
//!    schema is sequenced by hope.
//!
//! ## Scope
//!
//! The declaration and its ordering/cleanup semantics. Execution binds to the
//! environment seam; the interpreter arm is the caller's, exactly as assertions
//! work today.

use serde::{Deserialize, Serialize};

/// What a fixture does to the environment.
///
/// The distinction assertions do not have: an assertion OBSERVES, a fixture
/// CHANGES. Making it a type means a run can report what it altered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Effect {
    /// Creates state that did not exist. Cleanup removes it.
    Create,
    /// Modifies state that already exists. Cleanup CANNOT restore it, which is
    /// why it is its own variant rather than a flavour of `Create` — see
    /// [`Fixture::is_reversible`].
    Mutate,
}

/// How a fixture is undone when the trial ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cleanup {
    /// The environment is destroyed after the trial, so nothing needs undoing.
    ///
    /// Honest for a viveiro ephemeral environment and dangerous anywhere else:
    /// declaring this against a CLAIMED environment that will be released back
    /// to a pool leaves the fixture for the next claimant.
    EnvironmentIsDestroyed,
    /// Undo by placing this inverse fixture. Ordered after the trial, in
    /// reverse declaration order.
    Inverse(Box<Fixture>),
    /// Deliberately left behind, with a reason. Requires saying why, so
    /// "we never cleaned this up" cannot masquerade as a decision.
    Retained { reason: String },
    /// Put the object back to the value captured in the trial's natural state
    /// (`crate::visita`). The inverse is DERIVED from the capture, not chosen
    /// by the author — which is what makes a [`Effect::Mutate`] reversible:
    /// the value restored is the value that was there, and the restore diff
    /// proves it.
    RestoreFromSnapshot,
    /// Nothing to undo: the effect decays by itself (a closed connection is
    /// re-opened by its client). Requires saying how, and the trial's restore
    /// diff still has to come back empty.
    Transient { reason: String },
    /// Stop what the fixture started if it is still running — cancel a
    /// dispatched pipeline run. Runs before any other undo, so nothing is put
    /// back while something the trial started can still change it again.
    Stop,
    /// Undo by the action's own inverse, which it carries or records: an
    /// applied manifest deletes exactly the objects it created.
    OwnInverse,
}

/// When a fixture is undone, relative to the trial's restore.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UndoPhase {
    /// With the others, in reverse placement order.
    #[default]
    InOrder,
    /// Only after the restore fixtures ran and the restore check converged —
    /// for something the restore still needs, such as a proxy that clients
    /// are pointed at until the restore points them back. Held objects are
    /// left out of that check; the final diff, after this undo, must find
    /// them gone. Also last on a guard trip.
    AfterRestore,
}

impl UndoPhase {
    #[must_use]
    pub fn is_in_order(&self) -> bool {
        matches!(self, Self::InOrder)
    }
}

/// One declared piece of per-experiment data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fixture {
    /// Identifies this fixture in reports and in ordering.
    pub name: String,
    /// What it does to the environment.
    pub effect: Effect,
    /// How it is undone.
    pub cleanup: Cleanup,
    /// Fixture names that must be placed before this one.
    ///
    /// Ordering is DECLARED, never inferred from position in the list: a
    /// fixture that needs the schema, or another fixture's output, says so.
    #[serde(default)]
    pub after: Vec<String>,
    /// What placing this does, typed — executed by an environment that knows
    /// how (`crate::visita::OperatorEnvironment`). `None` keeps the historical
    /// meaning: the declaration only, execution bound elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<crate::visita::Action>,
    /// When it is undone. Default: in order.
    #[serde(
        default,
        rename = "undoPhase",
        skip_serializing_if = "UndoPhase::is_in_order"
    )]
    pub undo_phase: UndoPhase,
}

impl Fixture {
    /// Whether placing this can be undone.
    ///
    /// `Mutate` is not reversible with an `Inverse`: the inverse restores a
    /// value someone chose, not the value that was there. It IS reversible
    /// with `RestoreFromSnapshot`, whose inverse is the captured value.
    /// Surfacing that keeps "we cleaned up" from meaning two different things.
    #[must_use]
    pub fn is_reversible(&self) -> bool {
        match (&self.effect, &self.cleanup) {
            (Effect::Create, Cleanup::Inverse(_) | Cleanup::EnvironmentIsDestroyed)
            | (_, Cleanup::RestoreFromSnapshot | Cleanup::OwnInverse) => true,
            (Effect::Mutate, Cleanup::Inverse(_) | Cleanup::EnvironmentIsDestroyed)
            // Stopping a run does not reverse what it already did.
            | (_, Cleanup::Retained { .. } | Cleanup::Transient { .. } | Cleanup::Stop) => false,
        }
    }

    /// Whether this fixture leaves state behind after the trial.
    #[must_use]
    pub fn leaves_residue(&self) -> bool {
        matches!(self.cleanup, Cleanup::Retained { .. })
    }
}

/// Why a fixture set could not be ordered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixtureOrderError {
    /// `after` names a fixture that is not declared.
    UnknownDependency { fixture: String, missing: String },
    /// The `after` graph has a cycle, named.
    Cycle { involved: Vec<String> },
    /// Two fixtures share a name, so `after` cannot address them
    /// unambiguously.
    DuplicateName { name: String },
}

impl core::fmt::Display for FixtureOrderError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownDependency { fixture, missing } => write!(
                f,
                "fixture `{fixture}` must come after `{missing}`, which is not declared"
            ),
            Self::Cycle { involved } => {
                write!(f, "fixture ordering has a cycle: {}", involved.join(" -> "))
            }
            Self::DuplicateName { name } => {
                write!(f, "two fixtures are both named `{name}`")
            }
        }
    }
}

/// Order fixtures for placement: dependencies first, declaration order
/// otherwise.
///
/// # Errors
///
/// [`FixtureOrderError`] for an unknown dependency, a cycle, or a duplicate
/// name. A cycle is REFUSED rather than broken arbitrarily — silently picking
/// an order would place data in a sequence nobody declared, and the resulting
/// trial would be unreproducible in the one way that matters.
pub fn placement_order(fixtures: &[Fixture]) -> Result<Vec<&Fixture>, FixtureOrderError> {
    let mut seen = std::collections::BTreeSet::new();
    for f in fixtures {
        if !seen.insert(&f.name) {
            return Err(FixtureOrderError::DuplicateName {
                name: f.name.clone(),
            });
        }
    }
    for f in fixtures {
        for dep in &f.after {
            if !fixtures.iter().any(|o| &o.name == dep) {
                return Err(FixtureOrderError::UnknownDependency {
                    fixture: f.name.clone(),
                    missing: dep.clone(),
                });
            }
        }
    }

    let mut ordered: Vec<&Fixture> = Vec::with_capacity(fixtures.len());
    let mut placed: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();

    while ordered.len() < fixtures.len() {
        let before = ordered.len();
        for f in fixtures {
            if placed.contains(f.name.as_str()) {
                continue;
            }
            if f.after.iter().all(|d| placed.contains(d.as_str())) {
                ordered.push(f);
                placed.insert(f.name.as_str());
            }
        }
        if ordered.len() == before {
            let mut involved: Vec<String> = fixtures
                .iter()
                .filter(|f| !placed.contains(f.name.as_str()))
                .map(|f| f.name.clone())
                .collect();
            involved.sort();
            return Err(FixtureOrderError::Cycle { involved });
        }
    }
    Ok(ordered)
}

/// The order fixtures are undone: the reverse of placement.
///
/// Reverse, not declaration order: a fixture placed after another may depend on
/// it, so undoing in declaration order can remove a dependency out from under
/// something still present.
///
/// # Errors
///
/// Propagates [`placement_order`]'s errors.
pub fn cleanup_order(fixtures: &[Fixture]) -> Result<Vec<&Fixture>, FixtureOrderError> {
    let mut o = placement_order(fixtures)?;
    o.reverse();
    Ok(o)
}

/// Everything a trial will leave behind, for the run's report.
///
/// Carried as a first-class result rather than discovered later: an ephemeral
/// environment that is destroyed makes residue harmless, and a CLAIMED one
/// makes it the next trial's problem.
#[must_use]
pub fn residue(fixtures: &[Fixture]) -> Vec<&Fixture> {
    fixtures.iter().filter(|f| f.leaves_residue()).collect()
}

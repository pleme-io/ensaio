//! Ensaio's interpreter.  `apply` walks an [`EnsaioSpec`], dispatching
//! every side effect (provisioning, wiring, running an assertion,
//! recording, tearing down) through an [`EnsaioEnvironment`].  Real
//! implementations talk to Kubernetes; tests mock the trait.

// The `EnsaioEnvironment` trait methods all return `Result<_, String>` whose
// error is the environment's own opaque message — a per-method `# Errors`
// section would only restate that. Scoped allow for the trait's IO seam.
#![allow(clippy::missing_errors_doc)]

use crate::error::EnsaioError;
use crate::fixture::{Cleanup, Fixture, cleanup_order, placement_order, residue};
use crate::nervous::PhaseEvent;
use crate::types::{
    AssertionKind, AssertionResult, AssertionSpec, EnsaioOutcome, EnsaioSpec, FixtureResidue,
    SystemSpec, Verdict, WiringSpec,
};

/// Abstract IO for an Ensaio run.  Every side-effecting step of the
/// rig lives here so `apply` stays pure and the whole pipeline is
/// mockable without a live cluster.
pub trait EnsaioEnvironment {
    /// Stand up one system from its template.  Returns an opaque
    /// endpoint/reference `apply` threads into later steps.
    fn provision_system(&self, sys: &SystemSpec) -> Result<String, String>;

    /// Block until the system referenced by `endpoint` is ready.
    fn wait_ready(&self, endpoint: &str) -> Result<(), String>;

    /// Apply a wiring edge between two provisioned systems.
    fn wire(&self, w: &WiringSpec) -> Result<(), String>;

    /// Place one per-experiment fixture — the layer ensaio owns.
    ///
    /// Called between wiring and assertions, in [`placement_order`]. No default
    /// impl, for the same reason [`Self::publish_phase_event`] has none: an
    /// environment that cannot place data must SAY so, and a defaulted `Ok(())`
    /// would be the exact silent no-op this seam exists to end — a declared
    /// fixture, a clean run, and nothing placed.
    fn place_fixture(&self, f: &Fixture) -> Result<(), String>;

    /// Undo one placed fixture.
    ///
    /// Called only for a fixture whose [`Cleanup`] is [`Cleanup::Inverse`]; the
    /// other two arms are decided by `apply` from the typed declaration and
    /// never reach an environment, so no impl has to re-derive the rule.
    fn clean_fixture(&self, f: &Fixture) -> Result<(), String>;

    /// Run one assertion, returning its verdict.
    fn run_assertion(&self, a: &AssertionSpec) -> Result<Verdict, String>;

    /// Record the run's outcome.  Default is a no-op — the M0/M1
    /// interim has no durable attestation format yet (unverified).
    fn record(&self, _outcome: &EnsaioOutcome) -> Result<(), String> {
        Ok(())
    }

    /// Tear down a provisioned system. For a raw, non-declared M0-shape
    /// system this deletes it. For a Helm/FluxCD-declared system (M2+)
    /// the destination is scale-to-zero (`Dormant`, a breathe
    /// `ReplicaBand` floor) — cost-free and instantly wakeable — never
    /// an actual delete: teardown is scale-to-zero, not delete.
    fn teardown_system(&self, endpoint: &str) -> Result<(), String>;

    /// Publish one typed phase-transition event onto the reactive
    /// nervous system (escuta/NATS) — the reactive-nervous-system
    /// extension point. Called once per `status.phase` commit by the M2
    /// controller (`controller::step_phase`). No default impl
    /// deliberately: every `EnsaioEnvironment` must decide explicitly
    /// how (or whether) it publishes, rather than silently inheriting a
    /// no-op — `MockEnv` records the call for tests; `K8sEnvironment`'s
    /// real NATS-backed publish is a named, honestly-stubbed follow-up
    /// (see `k8s_env.rs`). A publish failure is never fatal to a
    /// reconcile — callers log and continue.
    fn publish_phase_event(&self, event: &PhaseEvent) -> Result<(), String>;
}

/// Apply an `EnsaioSpec`: provision every system, wire them, place every
/// fixture, run every assertion, undo the fixtures, record the outcome, tear
/// everything down — the last three unconditionally, even on assertion failure
/// (teardown never depends on the verdict).
///
/// **Fixtures sit between wiring and assertions**, which is the only place they
/// can sit: a fixture may need a wired system to place into, and every assertion
/// is written against the data being there. The order within the set is
/// [`placement_order`] — DECLARED via `after`, never inferred from position —
/// and undoing is [`cleanup_order`], its reverse, so a fixture is never removed
/// out from under one that depends on it.
///
/// **A placement failure skips the assertions.** Judging a trial whose data is
/// half-placed produces a confident verdict about an environment nobody
/// declared, which is worse than no verdict. What WAS placed is still undone and
/// every system is still torn down.
///
/// # Errors
///
/// `EnsaioError::Interp` for any environment-trait failure during provisioning,
/// wiring, fixture placement/cleanup, or teardown — and for a fixture set that
/// cannot be ordered at all (an unknown `after`, a cycle, a duplicate name),
/// which is refused BEFORE anything is placed. Assertion failures are NOT
/// errors — a falsified claim is a valid, recorded outcome; only a broken *rig*
/// is an error.
pub fn apply<E: EnsaioEnvironment>(
    spec: &EnsaioSpec,
    env: &E,
) -> Result<EnsaioOutcome, EnsaioError> {
    // Ordered FIRST, before a single system is provisioned: an unorderable
    // fixture set is a defect in the spec, and discovering it after the rig is
    // standing costs a teardown to learn something that was decidable from the
    // text.
    let ordered: Vec<&Fixture> =
        placement_order(&spec.fixtures).map_err(|e| EnsaioError::Interp {
            phase: "fixtures".to_string(),
            message: e.to_string(),
        })?;

    let mut outcome = EnsaioOutcome {
        run_id: spec.name.clone(),
        ..Default::default()
    };

    let mut endpoints: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for sys in &spec.systems {
        let endpoint = env.provision_system(sys).map_err(|e| EnsaioError::Interp {
            phase: format!("provision:{}", sys.id),
            message: e,
        })?;
        env.wait_ready(&endpoint).map_err(|e| EnsaioError::Interp {
            phase: format!("wait-ready:{}", sys.id),
            message: e,
        })?;
        outcome.systems_provisioned.push(sys.id.clone());
        endpoints.insert(sys.id.clone(), endpoint);
    }

    for w in &spec.wiring {
        env.wire(w).map_err(|e| EnsaioError::Interp {
            phase: format!("wire:{}->{}", w.from, w.to),
            message: e,
        })?;
    }

    // ── place ───────────────────────────────────────────────────────
    //
    // `placed` is what the run must undo and report, and it is what actually
    // happened rather than what was declared: the loop stops at the first
    // failure, so a fixture after the break was never placed and must not be
    // cleaned (undoing something that was never done is its own mutation) nor
    // counted as residue.
    let mut placed: Vec<&Fixture> = Vec::new();
    let mut placement_failure: Option<EnsaioError> = None;
    for f in ordered {
        match env.place_fixture(f) {
            Ok(()) => {
                placed.push(f);
                outcome.fixtures_placed.push(f.name.clone());
            }
            Err(message) => {
                placement_failure = Some(EnsaioError::Interp {
                    phase: format!("place-fixture:{}", f.name),
                    message,
                });
                break;
            }
        }
    }

    // A half-placed environment is not the environment the assertions were
    // written against. Skip them rather than record a confident verdict about
    // something nobody declared — cleanup and teardown still run below.
    if placement_failure.is_none() {
        for a in &spec.assertions {
            let verdict = match env.run_assertion(a) {
                Ok(v) => v,
                Err(message) => Verdict::Errored { message },
            };
            outcome.results.push(AssertionResult {
                assertion_id: a.id.clone(),
                kind: a.kind,
                verdict,
            });
        }
    }

    // ── undo ────────────────────────────────────────────────────────
    //
    // Reverse of placement (`cleanup_order`), restricted to what was placed.
    // Best-effort like teardown: one fixture that will not come off must not
    // strand the rest.
    let mut cleanup_errors = Vec::new();
    let undo = cleanup_order(&spec.fixtures).map_err(|e| EnsaioError::Interp {
        phase: "fixtures".to_string(),
        message: e.to_string(),
    })?;
    for f in undo {
        if !placed.iter().any(|p| p.name == f.name) {
            continue;
        }
        // EXHAUSTIVE on purpose. Which cleanups need an action is decided here,
        // from the typed declaration, so no environment re-derives the rule —
        // and a new `Cleanup` variant is a compile error at this arm rather
        // than a mode that silently cleans nothing.
        match &f.cleanup {
            Cleanup::Inverse(_)
            | Cleanup::RestoreFromSnapshot
            | Cleanup::Stop
            | Cleanup::OwnInverse => match env.clean_fixture(f) {
                Ok(()) => outcome.fixtures_cleaned.push(f.name.clone()),
                Err(e) => cleanup_errors.push(format!("{}: {e}", f.name)),
            },
            // Decays on its own; the trial's own evidence is what shows it did.
            Cleanup::Transient { .. } => {}
            // The environment goes away whole; undoing one fixture inside it
            // first is work with no effect.
            Cleanup::EnvironmentIsDestroyed => {}
            // Deliberately left behind. Reported as residue below, never
            // silently dropped.
            Cleanup::Retained { .. } => {}
        }
    }

    outcome.residue = residue(&spec.fixtures)
        .into_iter()
        .filter(|f| placed.iter().any(|p| p.name == f.name))
        .map(|f| FixtureResidue {
            fixture: f.name.clone(),
            reason: match &f.cleanup {
                Cleanup::Retained { reason } => reason.clone(),
                // Unreachable through `residue`, which selects exactly the
                // `Retained` arm — stated rather than unwrapped so a widened
                // predicate cannot turn into a panic inside a run.
                Cleanup::Inverse(_)
                | Cleanup::EnvironmentIsDestroyed
                | Cleanup::RestoreFromSnapshot
                | Cleanup::Transient { .. }
                | Cleanup::Stop
                | Cleanup::OwnInverse => "declared residue with no recorded reason".to_string(),
            },
        })
        .collect();

    if spec.record {
        env.record(&outcome).map_err(|e| EnsaioError::Interp {
            phase: "record".into(),
            message: e,
        })?;
    }

    // Teardown runs unconditionally — even if an assertion or the
    // record step above surfaced an error, every provisioned system
    // still gets a teardown attempt. Best-effort: collect failures
    // rather than short-circuit on the first one.
    let mut teardown_errors = Vec::new();
    for sys in &spec.systems {
        if let Some(endpoint) = endpoints.get(&sys.id) {
            match env.teardown_system(endpoint) {
                Ok(()) => outcome.systems_torn_down.push(sys.id.clone()),
                Err(e) => teardown_errors.push(format!("{}: {e}", sys.id)),
            }
        }
    }
    if !teardown_errors.is_empty() {
        return Err(EnsaioError::Interp {
            phase: "teardown".into(),
            message: teardown_errors.join("; "),
        });
    }

    // Reported LAST, after every undo and teardown has run, but reported: a
    // placement failure is a broken rig, and returning `Ok` with an empty
    // `results` would read as a trial that simply asserted nothing.
    if let Some(e) = placement_failure {
        return Err(e);
    }
    if !cleanup_errors.is_empty() {
        return Err(EnsaioError::Interp {
            phase: "clean-fixture".into(),
            message: cleanup_errors.join("; "),
        });
    }

    Ok(outcome)
}

/// Map a kind that has no matching interpreter arm to a typed error
/// rather than a silent pass — the CATALOG REFLECTION invariant this
/// crate is built around. Real `EnsaioEnvironment::run_assertion`
/// implementations should call this for any `AssertionKind` they
/// don't yet handle.
///
/// # Errors
///
/// Always returns `EnsaioError::UnknownAssertionKind`.
pub fn unimplemented_kind(kind: AssertionKind) -> Result<Verdict, EnsaioError> {
    Err(EnsaioError::UnknownAssertionKind(format!("{kind:?}")))
}

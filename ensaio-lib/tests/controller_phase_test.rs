//! Unit tests for the M2 controller's phase-FSM core
//! (`ensaio::controller::step_phase`) driven directly against `MockEnv` —
//! no live cluster, no mock kube apiserver needed (that plumbing is
//! `controller_reconcile_test.rs`'s job).
//!
//! These tests exist specifically to prove the reactive-nervous-system
//! extension point: a typed
//! [`ensaio::PhaseEvent`] publish, behind
//! `EnsaioEnvironment::publish_phase_event`, fires exactly once per phase
//! transition `step_phase` commits — the same rigor the crate already
//! applies to provision/wire/assert/teardown in `interp_tests.rs`.

use ensaio::controller::{StepOutcome, step_phase};
use ensaio::crd::{EnsaioCrdSpec, EnsaioPhase, EnsaioStatus};
use ensaio::mock::MockEnv;
use ensaio::{AssertionKind, AssertionSpec, SystemSpec, WiringSpec};

const NS: &str = "trials-ensaio";
const NAME: &str = "test-run";

fn spec() -> EnsaioCrdSpec {
    EnsaioCrdSpec {
        scope: "trials".into(),
        ttl: "30m".into(),
        claim: "phase-fsm test claim".into(),
        confluence_ref: None,
        systems: vec![
            SystemSpec {
                id: "sys-a".into(),
                template: "nginx:1.27-alpine".into(),
                breathe: false,
            },
            SystemSpec {
                id: "sys-b".into(),
                template: "nginx:1.27-alpine".into(),
                breathe: false,
            },
        ],
        wiring: vec![WiringSpec {
            from: "sys-a".into(),
            to: "sys-b".into(),
            via: "cluster-dns".into(),
        }],
        assertions: vec![AssertionSpec {
            id: "a1".into(),
            kind: AssertionKind::NetworkPath,
            target_namespace: None,
            target: "sys-b".into(),
            proof_channel: None,
            session: None,
            header_name: None,
            from: None,
            must_not_traverse: None,
            marker: None,
            image: None,
            command: None,
            args: None,
            sidecar: None,
            env: None,
            probe_namespace: None,
            operator: ensaio::types::OperatorParams::default(),
        }],
        record: false,
        confirm_after_runs: 1,
    }
}

fn status_at(phase: EnsaioPhase) -> EnsaioStatus {
    EnsaioStatus {
        phase,
        ..Default::default()
    }
}

#[test]
fn pending_success_publishes_exactly_one_provisioning_event() {
    let env = MockEnv::new();
    let spec = spec();
    let mut status = status_at(EnsaioPhase::Pending);

    let outcome = step_phase(&env, NS, NAME, &spec, &mut status).expect("must succeed");

    assert_eq!(status.phase, EnsaioPhase::Provisioning);
    assert!(matches!(outcome, StepOutcome::Requeue(_)));

    let events = env.phase_events.borrow();
    assert_eq!(events.len(), 1, "exactly one publish per transition");
    assert_eq!(events[0].run, NAME);
    assert_eq!(events[0].namespace, NS);
    assert_eq!(events[0].phase, EnsaioPhase::Provisioning);
    assert_eq!(events[0].subject(), "ensaio.test-run.provisioning");
}

#[test]
fn pending_provision_failure_transitions_to_failed_and_publishes_it() {
    let env = MockEnv::new().failing_provision("sys-a");
    let spec = spec();
    let mut status = status_at(EnsaioPhase::Pending);

    let outcome = step_phase(&env, NS, NAME, &spec, &mut status).expect("must succeed");

    assert_eq!(status.phase, EnsaioPhase::Failed);
    assert_eq!(outcome, StepOutcome::AwaitChange);

    let events = env.phase_events.borrow();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].phase, EnsaioPhase::Failed);
    assert_eq!(events[0].subject(), "ensaio.test-run.failed");
}

#[test]
fn a_terminal_phase_publishes_once_and_awaits_change() {
    let env = MockEnv::new();
    let spec = spec();
    let mut status = status_at(EnsaioPhase::Done);

    let outcome = step_phase(&env, NS, NAME, &spec, &mut status).expect("must succeed");

    assert_eq!(status.phase, EnsaioPhase::Done);
    assert_eq!(outcome, StepOutcome::AwaitChange);
    assert_eq!(env.phase_events.borrow().len(), 1);
    assert_eq!(
        env.phase_events.borrow()[0].subject(),
        "ensaio.test-run.done"
    );
}

#[test]
fn ready_phase_wires_then_publishes_the_wiring_transition() {
    // `MockEnv::wire` has no configurable failure hook, so the
    // `?`-propagated `ControllerError::Rig` path preserved from the
    // pre-refactor `apply_reconcile` (a wire failure short-circuits
    // before any `status` mutation or publish) is not exercisable here
    // — it is unchanged, load-bearing existing behavior, not something
    // this pass introduces. This test covers the success path: the
    // Ready -> Wiring transition commits and publishes exactly once.
    let env = MockEnv::new();
    let spec = spec();
    let mut status = status_at(EnsaioPhase::Ready);

    let outcome = step_phase(&env, NS, NAME, &spec, &mut status).expect("must succeed");
    assert_eq!(status.phase, EnsaioPhase::Wiring);
    assert_eq!(
        outcome,
        StepOutcome::Requeue(std::time::Duration::from_secs(5))
    );
    assert_eq!(env.phase_events.borrow().len(), 1);
    assert_eq!(env.phase_events.borrow()[0].phase, EnsaioPhase::Wiring);
}

#[test]
fn a_publish_failure_is_non_fatal_to_the_reconcile_step() {
    let env = MockEnv::new().failing_publish();
    let spec = spec();
    let mut status = status_at(EnsaioPhase::Pending);

    // Must still succeed and still advance the phase — a nervous-system
    // publish failure is observability, not a correctness dependency.
    let outcome = step_phase(&env, NS, NAME, &spec, &mut status).expect("must succeed");
    assert_eq!(status.phase, EnsaioPhase::Provisioning);
    assert!(matches!(outcome, StepOutcome::Requeue(_)));
    // The call still happened (and was recorded) even though it "failed".
    assert_eq!(env.phase_events.borrow().len(), 1);
}

#[test]
fn a_full_run_publishes_one_event_per_phase_in_order() {
    // Drive step_phase repeatedly, feeding the mutated status back in —
    // the same shape the real reconcile loop uses across successive
    // watch events — through the whole Pending..Done walk, proving the
    // publish fires at every transition in the right order.
    let env = MockEnv::new();
    let spec = spec();
    let mut status = EnsaioStatus::default(); // starts at Pending (#[default])

    let mut phases_seen = Vec::new();
    loop {
        step_phase(&env, NS, NAME, &spec, &mut status).expect("must succeed");
        phases_seen.push(status.phase);
        if status.phase == EnsaioPhase::Done {
            break;
        }
        assert!(phases_seen.len() < 20, "FSM did not converge to Done");
    }

    assert_eq!(
        phases_seen,
        vec![
            EnsaioPhase::Provisioning,
            EnsaioPhase::Ready,
            EnsaioPhase::Wiring,
            EnsaioPhase::Asserting,
            EnsaioPhase::Settled,
            EnsaioPhase::Done,
        ]
    );

    let events = env.phase_events.borrow();
    assert_eq!(
        events.len(),
        phases_seen.len(),
        "one publish per transition"
    );
    let subjects: Vec<String> = events.iter().map(|e| e.subject()).collect();
    assert_eq!(
        subjects,
        vec![
            "ensaio.test-run.provisioning",
            "ensaio.test-run.ready",
            "ensaio.test-run.wiring",
            "ensaio.test-run.asserting",
            "ensaio.test-run.settled",
            "ensaio.test-run.done",
        ]
    );
}

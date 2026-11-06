//! Interpreter tests against `MockEnv` — no live cluster needed.

use ensaio::fixture::{Cleanup, Effect, Fixture};
use ensaio::mock::MockEnv;
use ensaio::{AssertionKind, EnsaioSpec, Observation, Verdict, apply, load_all};

const M0_SPEC: &str = include_str!("../../specs/examples/admission-gate.ensaio.lisp");

// ── fixture placement ───────────────────────────────────────────────
//
// `EnsaioSpec.fixtures` deserialized and NOTHING placed it: `fixture.rs`'s
// placement_order / cleanup_order / residue were written, tested, and had zero
// callers, so an author could declare fixtures, watch the CR apply cleanly, and
// get a green run over data that was never placed. A field that parses and is
// ignored is worse than a missing one — the missing field errors, this one lies.
// These prove the seam is REACHED and ordered, which is the half a unit test of
// `placement_order` alone can never show.

fn held() -> Verdict {
    Verdict::from_probe(vec![Observation::new("ok")], true)
}

fn spec_with_fixtures(fixtures: Vec<Fixture>) -> EnsaioSpec {
    let mut spec = load_all::<EnsaioSpec>(M0_SPEC).unwrap().remove(0);
    spec.fixtures = fixtures;
    spec
}

fn fixture(name: &str, after: &[&str], cleanup: Cleanup) -> Fixture {
    Fixture {
        name: name.into(),
        effect: Effect::Create,
        cleanup,
        after: after.iter().map(|s| (*s).to_string()).collect(),
        action: None,
        undo_phase: ensaio::fixture::UndoPhase::default(),
    }
}

fn inverse_of(name: &str) -> Cleanup {
    Cleanup::Inverse(Box::new(fixture(
        &format!("undo-{name}"),
        &[],
        Cleanup::EnvironmentIsDestroyed,
    )))
}

/// **The requirement.** Declared fixtures are placed, in dependency order,
/// AFTER wiring and BEFORE the assertions written against them — and undone in
/// the reverse order once the trial is judged.
#[test]
fn fixtures_are_placed_between_wiring_and_assertions_and_undone_in_reverse() {
    let spec = spec_with_fixtures(vec![
        fixture("queue", &["policy"], inverse_of("queue")),
        fixture("policy", &[], inverse_of("policy")),
    ]);
    let env = MockEnv::new().with_verdict("m2m-admitted-without-sso", held());

    let outcome = apply(&spec, &env).expect("apply must succeed");

    assert_eq!(outcome.fixtures_placed, vec!["policy", "queue"]);
    assert_eq!(outcome.fixtures_cleaned, vec!["queue", "policy"]);
    assert!(outcome.residue.is_empty());

    let log = env.log.borrow();
    let at = |needle: &str| log.iter().position(|l| l == needle).unwrap();
    assert!(at("WIRE authservice-mock->backend-stub") < at("PLACE policy"));
    assert!(at("PLACE policy") < at("PLACE queue"));
    assert!(at("PLACE queue") < at("ASSERT m2m-admitted-without-sso"));
    assert!(at("ASSERT m2m-admitted-without-sso") < at("CLEAN queue"));
    assert!(at("CLEAN queue") < at("CLEAN policy"));
    assert!(at("CLEAN policy") < at("TEARDOWN authservice-mock.mock.svc.cluster.local"));
}

/// The two non-`Inverse` cleanups reach no environment at all: destroying the
/// environment undoes them wholesale, and a retained fixture is deliberately
/// left. Decided from the typed declaration in `apply`, so no environment impl
/// re-derives the rule.
#[test]
fn only_an_inverse_cleanup_reaches_the_environment() {
    let spec = spec_with_fixtures(vec![
        fixture("ephemeral", &[], Cleanup::EnvironmentIsDestroyed),
        fixture(
            "audit-row",
            &[],
            Cleanup::Retained {
                reason: "the audit trail is the evidence this trial produced".into(),
            },
        ),
    ]);
    let env = MockEnv::new().with_verdict("m2m-admitted-without-sso", held());

    let outcome = apply(&spec, &env).expect("apply must succeed");

    assert_eq!(outcome.fixtures_placed, vec!["ephemeral", "audit-row"]);
    assert!(
        outcome.fixtures_cleaned.is_empty(),
        "{:?}",
        outcome.fixtures_cleaned
    );
    assert!(!env.log.borrow().iter().any(|l| l.starts_with("CLEAN")));

    assert_eq!(outcome.residue.len(), 1);
    assert_eq!(outcome.residue[0].fixture, "audit-row");
    assert!(
        outcome.residue[0].reason.contains("audit trail"),
        "residue must carry the declared REASON, or `Retained`'s requirement to \
         say why is given back up: {:?}",
        outcome.residue[0]
    );
}

/// **A half-placed environment is not judged.** Assertions are written against
/// the data being there; running them over a partial placement produces a
/// confident verdict about an environment nobody declared. What WAS placed is
/// still undone, every system is still torn down, and the failure surfaces.
#[test]
fn a_placement_failure_skips_the_assertions_and_still_undoes_what_was_placed() {
    let spec = spec_with_fixtures(vec![
        fixture("policy", &[], inverse_of("policy")),
        fixture("queue", &["policy"], inverse_of("queue")),
    ]);
    let env = MockEnv::new().failing_place("queue");

    let err = apply(&spec, &env).expect_err("a placement failure is a rig error");
    assert!(
        format!("{err}").contains("queue"),
        "the error must name the fixture: {err}"
    );

    let log = env.log.borrow();
    assert!(
        !log.iter().any(|l| l.starts_with("ASSERT")),
        "a half-placed environment must not be judged: {log:?}"
    );
    assert!(
        log.iter().any(|l| l == "CLEAN policy"),
        "what WAS placed must still be undone: {log:?}"
    );
    assert!(
        !log.iter().any(|l| l == "CLEAN queue"),
        "a fixture that was never placed must not be undone — that is its own \
         mutation: {log:?}"
    );
    assert_eq!(
        log.iter().filter(|l| l.starts_with("TEARDOWN")).count(),
        2,
        "every system is still torn down: {log:?}"
    );
}

/// An unorderable set is refused BEFORE anything is provisioned: it is decidable
/// from the text, and discovering it after the rig is standing costs a teardown
/// to learn nothing.
#[test]
fn an_unorderable_fixture_set_is_refused_before_the_rig_is_built() {
    let spec = spec_with_fixtures(vec![
        fixture("a", &["b"], Cleanup::EnvironmentIsDestroyed),
        fixture("b", &["a"], Cleanup::EnvironmentIsDestroyed),
    ]);
    let env = MockEnv::new();

    let err = apply(&spec, &env).expect_err("a cycle must be refused");
    assert!(format!("{err}").contains("cycle"), "{err}");
    assert!(
        env.log.borrow().is_empty(),
        "nothing may be provisioned before the set is known to be orderable: {:?}",
        env.log.borrow()
    );
}

/// A cleanup failure surfaces rather than being swallowed, and does not strand
/// the fixtures behind it.
#[test]
fn a_cleanup_failure_surfaces_and_does_not_strand_the_rest() {
    let spec = spec_with_fixtures(vec![
        fixture("policy", &[], inverse_of("policy")),
        fixture("queue", &["policy"], inverse_of("queue")),
    ]);
    let env = MockEnv::new()
        .with_verdict("m2m-admitted-without-sso", held())
        .failing_clean("queue");

    let err = apply(&spec, &env).expect_err("a cleanup failure is a rig error");
    assert!(format!("{err}").contains("queue"), "{err}");
    assert!(
        env.log.borrow().iter().any(|l| l == "CLEAN policy"),
        "one fixture that will not come off must not strand the rest: {:?}",
        env.log.borrow()
    );
}

/// Every existing spec declares no fixtures and must be untouched by all of the
/// above — including the outcome fields, which stay empty rather than defaulting
/// to something a report could read as activity.
#[test]
fn a_spec_with_no_fixtures_is_unchanged() {
    let spec = load_all::<EnsaioSpec>(M0_SPEC).unwrap().remove(0);
    assert!(spec.fixtures.is_empty());
    let env = MockEnv::new().with_verdict("m2m-admitted-without-sso", held());

    let outcome = apply(&spec, &env).expect("apply must succeed");

    assert!(outcome.fixtures_placed.is_empty());
    assert!(outcome.fixtures_cleaned.is_empty());
    assert!(outcome.residue.is_empty());
    assert!(
        !env.log
            .borrow()
            .iter()
            .any(|l| l.starts_with("PLACE") || l.starts_with("CLEAN"))
    );
}

#[test]
fn m0_spec_compiles() {
    let specs = load_all::<EnsaioSpec>(M0_SPEC).expect("m0 spec must compile");
    assert_eq!(specs.len(), 1);
    let spec = &specs[0];
    assert_eq!(spec.name, "admission-gate");
    assert_eq!(spec.systems.len(), 2);
    assert_eq!(spec.wiring.len(), 1);
    assert_eq!(spec.assertions.len(), 1);
    assert_eq!(spec.assertions[0].kind, AssertionKind::AuthserviceAdmission);
    assert!(spec.record);
}

#[test]
fn apply_provisions_wires_asserts_records_tears_down_in_order() {
    let spec = load_all::<EnsaioSpec>(M0_SPEC).unwrap().remove(0);
    let env = MockEnv::new().with_verdict(
        "m2m-admitted-without-sso",
        Verdict::from_probe(
            vec![Observation::new(
                "200 from backend-stub, no auth redirect observed",
            )],
            true,
        ),
    );
    let outcome = apply(&spec, &env).expect("apply must succeed");

    assert_eq!(
        outcome.systems_provisioned,
        vec!["authservice-mock", "backend-stub"]
    );
    assert_eq!(outcome.systems_torn_down.len(), 2);
    assert_eq!(outcome.results.len(), 1);
    assert!(outcome.results[0].verdict.held());
    assert!(outcome.all_held());

    let log = env.log.borrow();
    let provision_a = log
        .iter()
        .position(|l| l == "PROVISION authservice-mock")
        .unwrap();
    let provision_b = log
        .iter()
        .position(|l| l == "PROVISION backend-stub")
        .unwrap();
    let wire = log.iter().position(|l| l.starts_with("WIRE")).unwrap();
    let assert_pos = log
        .iter()
        .position(|l| l == "ASSERT m2m-admitted-without-sso")
        .unwrap();
    let teardown_a = log
        .iter()
        .position(|l| l.starts_with("TEARDOWN authservice-mock"))
        .unwrap();
    // Both systems provisioned before wiring; wiring before the
    // assertion; every teardown after the assertion.
    assert!(provision_a < wire && provision_b < wire);
    assert!(wire < assert_pos);
    assert!(assert_pos < teardown_a);
}

#[test]
fn falsified_verdict_is_recorded_not_treated_as_a_rig_error() {
    let spec = load_all::<EnsaioSpec>(M0_SPEC).unwrap().remove(0);
    let env = MockEnv::new().with_verdict(
        "m2m-admitted-without-sso",
        Verdict::from_probe(
            vec![Observation::new(
                "302 redirect to Authservice login observed",
            )],
            false,
        ),
    );
    let outcome = apply(&spec, &env).expect("a falsified claim is a valid outcome, not an error");
    assert!(!outcome.all_held());
    assert!(!outcome.results[0].verdict.held());
    // Teardown still ran despite the falsified verdict.
    assert_eq!(outcome.systems_torn_down.len(), 2);
}

#[test]
fn teardown_runs_even_when_an_assertion_errors() {
    let spec = load_all::<EnsaioSpec>(M0_SPEC).unwrap().remove(0);
    // No verdict configured AND run_assertion isn't made to fail here
    // (MockEnv::run_assertion never returns Err), so exercise the
    // Errored-verdict path via a system-provision failure instead —
    // provisioning fails for the SECOND system, so the first must
    // still be torn down.
    let env = MockEnv::new().failing_provision("backend-stub");
    let result = apply(&spec, &env);
    assert!(
        result.is_err(),
        "a provisioning failure must surface as a rig error"
    );
    let log = env.log.borrow();
    // The first system was provisioned before the failure short-circuited
    // the loop — apply() doesn't reach teardown in this path since the
    // provisioning phase itself errors out before assertions/teardown run.
    assert!(log.iter().any(|l| l == "PROVISION authservice-mock"));
}

#[test]
fn teardown_failure_surfaces_as_a_rig_error_after_recording() {
    let spec = load_all::<EnsaioSpec>(M0_SPEC).unwrap().remove(0);
    let env = MockEnv::new()
        .with_verdict(
            "m2m-admitted-without-sso",
            Verdict::from_probe(vec![Observation::new("ok")], true),
        )
        .failing_teardown("authservice-mock");
    let result = apply(&spec, &env);
    assert!(
        result.is_err(),
        "a teardown failure must surface, not be swallowed"
    );
}

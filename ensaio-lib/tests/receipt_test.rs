//! `CertificationReceipt` projection — mock-green (pure, no cluster).
//! Proves the gate-honesty guarantees: preconditions gate, known gaps do
//! not, `Skipped` never becomes `Falsified`, and the content-address is
//! stable + verdict-sensitive.

use ensaio::{
    AssertionKind, AssertionResult, CertVerdict, CertificationInput, CertificationReceipt,
    KnownGap, Observation, Verdict,
};

fn result(id: &str, verdict: Verdict) -> AssertionResult {
    AssertionResult {
        assertion_id: id.into(),
        kind: AssertionKind::CustomJob,
        verdict,
    }
}

/// Every verdict here is minted through `Verdict::from_probe` —
/// `Verdict::Held { .. }` is `#[non_exhaustive]`, so this test crate
/// literally cannot name it (`E0639`). See `tests/derived_verdict_law.rs`.
fn held(id: &str) -> AssertionResult {
    result(id, Verdict::from_probe(vec![Observation::new("ok")], true))
}

fn falsified(id: &str) -> AssertionResult {
    result(
        id,
        Verdict::from_probe(vec![Observation::new("404")], false),
    )
}

fn input(preconditions: &[&str], gaps: Vec<KnownGap>) -> CertificationInput {
    CertificationInput {
        run_id: "rabbitmq-cert".into(),
        scope: "trials".into(),
        precondition_ids: preconditions.iter().map(|s| (*s).to_string()).collect(),
        known_gaps: gaps,
        at: "2026-07-25T00:00:00Z".into(),
    }
}

#[test]
fn all_held_with_precondition_held_is_certified() {
    let results = vec![
        held("mgmt-api-reachable"),
        held("vhost-app-exists"),
        held("plugins-enabled"),
    ];
    let r = CertificationReceipt::build(input(&["mgmt-api-reachable"], vec![]), &results);
    assert_eq!(r.verdict, CertVerdict::Certified);
    assert_eq!(r.env_preconditions.len(), 1);
    assert_eq!(r.artifact_assertions.len(), 2);
    assert_eq!(r.held_count(), 2);
    assert_eq!(r.falsified_count(), 0);
}

#[test]
fn one_falsified_artifact_is_not_certified() {
    let results = vec![
        held("mgmt-api-reachable"),
        held("vhost-app-exists"),
        falsified("vhost-dynamic-secret-exists"),
    ];
    let r = CertificationReceipt::build(input(&["mgmt-api-reachable"], vec![]), &results);
    assert_eq!(r.verdict, CertVerdict::NotCertified);
    assert_eq!(r.falsified_count(), 1);
    // A falsified artifact stays Falsified — never rounded to Skipped.
    assert!(
        r.artifact_assertions
            .iter()
            .any(|a| a.assertion_id == "vhost-dynamic-secret-exists"
                && matches!(a.verdict, Verdict::Falsified { .. }))
    );
}

#[test]
fn unmet_precondition_skips_artifacts_and_is_inconclusive() {
    // The broker's mgmt API is unreachable: the substrate wasn't fit to
    // test the topology, so every artifact is Skipped, NOT Falsified.
    let results = vec![
        falsified("mgmt-api-reachable"),
        held("vhost-app-exists"),
        falsified("vhost-dynamic-secret-exists"),
    ];
    let r = CertificationReceipt::build(input(&["mgmt-api-reachable"], vec![]), &results);
    assert_eq!(r.verdict, CertVerdict::Inconclusive);
    // No artifact contributes a falsification once the precondition failed.
    assert_eq!(r.falsified_count(), 0);
    assert!(
        r.artifact_assertions
            .iter()
            .all(|a| matches!(a.verdict, Verdict::Skipped { .. })),
        "every artifact routed to Skipped under an unmet precondition"
    );
}

#[test]
fn known_gaps_never_gate_the_verdict() {
    // A pre-existing application service gap is reported but does NOT
    // stop a rabbit-topology pass from certifying.
    let results = vec![held("mgmt-api-reachable"), held("vhost-app-exists")];
    let gaps = vec![KnownGap {
        id: "worker-crashloop".into(),
        description: "worker conf gap, independent of the broker".into(),
    }];
    let r = CertificationReceipt::build(input(&["mgmt-api-reachable"], gaps), &results);
    assert_eq!(r.verdict, CertVerdict::Certified);
    assert_eq!(r.env_known_gaps.len(), 1);
    assert_eq!(r.env_known_gaps[0].id, "worker-crashloop");
}

#[test]
fn empty_artifacts_is_inconclusive_not_certified() {
    // Only a precondition, nothing to certify => neither proven nor
    // disproven.
    let results = vec![held("mgmt-api-reachable")];
    let r = CertificationReceipt::build(input(&["mgmt-api-reachable"], vec![]), &results);
    assert_eq!(r.verdict, CertVerdict::Inconclusive);
}

#[test]
fn receipt_sha_is_stable_for_the_same_verdict_pattern() {
    let a = vec![held("mgmt-api-reachable"), held("vhost-app-exists")];
    let b = vec![
        result(
            "mgmt-api-reachable",
            Verdict::from_probe(vec![Observation::new("different noisy evidence")], true),
        ),
        result(
            "vhost-app-exists",
            Verdict::from_probe(vec![Observation::new("another run's logs")], true),
        ),
    ];
    let ra = CertificationReceipt::build(input(&["mgmt-api-reachable"], vec![]), &a);
    let rb = CertificationReceipt::build(input(&["mgmt-api-reachable"], vec![]), &b);
    // Same (id, verdict_class) pattern across two runs => same sha, even
    // though the evidence text differs (the sha is diffable across runs).
    assert_eq!(ra.receipt_sha, rb.receipt_sha);
    assert!(!ra.receipt_sha.is_empty());
}

#[test]
fn receipt_sha_changes_when_a_verdict_class_changes() {
    let held_run = vec![
        held("mgmt-api-reachable"),
        held("vhost-dynamic-secret-exists"),
    ];
    let falsified_run = vec![
        held("mgmt-api-reachable"),
        falsified("vhost-dynamic-secret-exists"),
    ];
    let ra = CertificationReceipt::build(input(&["mgmt-api-reachable"], vec![]), &held_run);
    let rb = CertificationReceipt::build(input(&["mgmt-api-reachable"], vec![]), &falsified_run);
    assert_ne!(
        ra.receipt_sha, rb.receipt_sha,
        "a Held->Falsified change must change the content-address"
    );
}

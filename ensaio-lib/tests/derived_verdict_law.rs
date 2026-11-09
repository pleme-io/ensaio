//! The DERIVED-VERDICT law applied to
//! ensaio's own product: **the verdict**.
//!
//! > A verdict is derived from the subject set it claims about, and carries
//! > the witness of that derivation.
//!
//! Two clauses, both required:
//!
//! 1. **Derivation** — one classifier computes the verdict; no constructor
//!    accepts one.
//! 2. **Witness** — the pass arm is non-constructible without a **non-empty**
//!    subject-set witness, and the empty case is a **distinct arm**, never a
//!    pass.
//!
//! ## The red run this file records
//!
//! Before the fix, `Verdict::Held { evidence: String::new() }` compiled and
//! `.held()` returned `true` — a trial reporting that an architectural
//! assertion HELD while carrying no evidence at all. There was also no arm
//! for a facet that examined an EMPTY subject set, so "held over 40
//! observations" and "held over zero" were the same value.
//!
//! The red-run receipt (a test asserting the defect was accepted, watched
//! passing against unfixed code) is recorded in the commit message. Every
//! test below is that receipt **inverted**: each one now pins the law.

use ensaio::{
    AssertionKind, AssertionResult, CertVerdict, CertificationInput, CertificationReceipt,
    Observation, Observations, Verdict,
};

// ── Clause 2: the pass arm requires a non-empty witness ────────────────

/// The inverted red run. `Verdict::judge` over an empty subject set can
/// never return `Held` — it returns the distinct `Vacuous` arm.
///
/// The stronger half of this claim cannot be written as a passing test at
/// all: the `Held` arm is `#[non_exhaustive]`, so naming it from this crate
/// is a **compile** error. That is not hypothetical — converting this very
/// test file plus `receipt_test.rs` produced, verbatim:
///
/// ```text
/// error[E0639]: cannot create non-exhaustive variant using struct expression
///   --> ensaio-lib/tests/receipt_test.rs:20:16
///    |
/// 20 |     result(id, Verdict::Held { evidence: "ok".into() })
///    |                ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
/// ```
///
/// An integration test is a separate crate, so that error IS the proof of
/// non-constructibility, recorded in this change's commit message. This
/// test pins the runtime half.
#[test]
fn empty_subject_set_is_vacuous_never_held() {
    let verdict = Verdict::judge(Observations::scope(Vec::new()), Vec::new());

    assert_eq!(
        verdict,
        Verdict::Vacuous {},
        "an empty scope must not be a pass"
    );
    assert!(!verdict.held(), "Vacuous must never round up to held");
    assert!(
        !verdict.falsified(),
        "Vacuous is not a falsification either — nothing was examined"
    );
}

/// A non-empty subject set with no findings is `Held`, and the witness
/// travels with the verdict: the verdict can name what it examined, and how
/// many.
#[test]
fn held_carries_its_examined_subject_set() {
    let verdict = Verdict::judge(
        Observations::scope(vec![
            Observation::new("GET http://broker/api/queues -> 200"),
            Observation::new("queue 'muda.events' present, durable=true"),
        ]),
        Vec::new(),
    );

    match &verdict {
        // `..` is required even to MATCH a `#[non_exhaustive]` variant from
        // another crate — the same attribute that makes construction an
        // `E0639` makes an exhaustive pattern an `E0638`.
        Verdict::Held { subjects, .. } => {
            assert_eq!(subjects.count().get(), 2, "the witness carries the count");
            assert!(
                subjects.iter().any(|o| o.as_str().contains("muda.events")),
                "the witness carries the examined values, not just a number"
            );
        }
        other => panic!("expected Held, got {other:?}"),
    }
    assert!(verdict.held());
}

/// Findings over a non-empty subject set are a falsification, and the
/// falsified arm carries BOTH the subject set and a non-empty finding set —
/// a `Falsified` with zero findings would be a `Held` wearing the wrong tag.
#[test]
fn findings_over_a_populated_domain_are_falsified_with_both_witnesses() {
    let verdict = Verdict::judge(
        Observations::scope(vec![Observation::new("GET /api/queues -> 200")]),
        vec![Observation::new("queue 'muda.events' absent")],
    );

    match &verdict {
        Verdict::Falsified {
            subjects, findings, ..
        } => {
            assert_eq!(subjects.count().get(), 1);
            assert_eq!(findings.count().get(), 1);
        }
        other => panic!("expected Falsified, got {other:?}"),
    }
    assert!(verdict.falsified());
    assert!(!verdict.held());
}

/// Findings over an EMPTY domain stay `Vacuous`. Emptiness is checked first,
/// so no finding count and no caller intent can promote an unexamined scope
/// to a claim of any kind.
#[test]
fn findings_over_an_empty_domain_stay_vacuous() {
    let verdict = Verdict::judge(
        Observations::scope(Vec::new()),
        vec![Observation::new("a finding about nothing")],
    );
    assert_eq!(verdict, Verdict::Vacuous {});
}

/// `Unreached` — the check never ran — is a distinct arm from `Vacuous` (the
/// check ran; nothing was in scope), and is the honest `Default` so a verdict
/// field that was never assigned reads as "no claim", never as a pass.
#[test]
fn unreached_is_distinct_from_vacuous_and_is_the_default() {
    assert_ne!(Verdict::Unreached {}, Verdict::Vacuous {});
    assert_eq!(Verdict::default(), Verdict::Unreached {});
    assert!(!Verdict::Unreached {}.held());
    assert!(!Verdict::default().held());
}

// ── Clause 2 at the wire border: a forged status does not deserialize ──

/// ensaio's verdict crosses a serde border — it lands in `status.results` on
/// a Kubernetes CR, which any actor with `patch status` can write. The
/// `#[serde(try_from)]` border **re-derives** the verdict from the carried
/// witness and rejects a record whose verdict and witness disagree, so a
/// forged `held` claim has no parse path.
#[test]
fn a_forged_held_over_an_empty_witness_is_rejected_at_the_wire() {
    // A hand-written status a hostile (or merely buggy) actor could patch in.
    let forged = r#"{"Held":{"subjects":[]}}"#;
    let parsed: Result<Verdict, _> = serde_json::from_str(forged);
    assert!(
        parsed.is_err(),
        "a held verdict over an empty subject set must not deserialize, got {parsed:?}"
    );
}

/// The re-derivation is not merely a non-emptiness check: a record tagged
/// `held` that *carries findings* disagrees with what `judge` would derive
/// from the same witness, and is rejected.
#[test]
fn a_forged_held_carrying_findings_is_rejected_at_the_wire() {
    let forged = r#"{"Held":{"subjects":["saw a thing"],"findings":["and it was wrong"]}}"#;
    let parsed: Result<Verdict, _> = serde_json::from_str(forged);
    assert!(
        parsed.is_err(),
        "held-with-findings must not deserialize, got {parsed:?}"
    );
}

/// An honest record round-trips. The border rejects forgeries, not traffic.
#[test]
fn an_honest_verdict_round_trips_through_the_wire() {
    for verdict in [
        Verdict::judge(
            Observations::scope(vec![Observation::new("probe succeeded")]),
            Vec::new(),
        ),
        Verdict::judge(
            Observations::scope(vec![Observation::new("probe ran")]),
            vec![Observation::new("exit 1")],
        ),
        Verdict::Vacuous {},
        Verdict::Unreached {},
        Verdict::Errored {
            message: "no tokio runtime".into(),
        },
        Verdict::Skipped {
            reason: "precondition unmet".into(),
        },
    ] {
        let json = serde_json::to_string(&verdict).expect("serialize");
        let back: Verdict = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(verdict, back, "round-trip must be lossless for {json}");
    }
}

// ── The live-CR migration: the blast radius, pinned ────────────────────

/// **The verbatim `status` of the one live `Ensaio` CR**, read off
/// a live EKS cluster on 2026-07-27:
/// `ensaio.pleme.io/v1alpha1 Ensaio eks-breathability-trial -n trials`
/// (`resourceVersion` 42802, `phase: Done`).
///
/// It carries the older externally-tagged shape. It is committed here as
/// a fixture because a `kube::runtime::Controller` watch decodes **every**
/// object in its stream: a parse border that rejected this record would not
/// merely refuse a forgery, it would wedge the trials controller on a CR
/// that has been sitting in the cluster since 2026-07-13.
const LIVE_CR_STATUS_RESULTS: &str = r#"[
  {"assertion_id":"all-nodes-spot","kind":"CustomJob",
   "verdict":{"Held":{"evidence":"job ensaio-probe-all-nodes-spot-jhzzn (alpine/k8s:1.31.1) succeeded\ncapacityTypes: SPOT SPOT\nall nodes SPOT\n"}}},
  {"assertion_id":"dual-pool-autoscale-groups-present","kind":"CustomJob",
   "verdict":{"Held":{"evidence":"job ensaio-probe-dual-pool-autoscale-groups-present-kjz2v (alpine/k8s:1.31.1) succeeded\npools: controllers system\nboth pools present\n"}}},
  {"assertion_id":"breathecloudpool-declared","kind":"CustomJob",
   "verdict":{"Held":{"evidence":"job ensaio-probe-breathecloudpool-declared-xtf46 (alpine/k8s:1.31.1) succeeded\nname=eks-pool floor=2 ceiling=5 lane=eks-managed-node-group\n"}}}
]"#;

/// The live CR still decodes, and its three legacy holds re-derive to real
/// `Held` verdicts carrying a one-observation witness — the old `evidence`
/// string is exactly the one thing each probe examined.
#[test]
fn the_live_eks_cr_status_still_deserializes() {
    let results: Vec<AssertionResult> = serde_json::from_str(LIVE_CR_STATUS_RESULTS)
        .expect("the live CR's status must still parse");

    assert_eq!(results.len(), 3);
    for r in &results {
        assert!(
            r.verdict.held(),
            "{} should re-derive to Held, got {:?}",
            r.assertion_id,
            r.verdict
        );
        assert_eq!(
            r.verdict.subject_count(),
            1,
            "the legacy evidence string is one observation"
        );
    }
}

/// **The migration corrects exactly the records that were lying.** A legacy
/// `Held` whose `evidence` is empty — the shipped defect — does not survive
/// the read as a pass: it re-derives to `Vacuous`, because an empty evidence
/// string scopes to an empty subject set.
#[test]
fn a_legacy_hold_over_empty_evidence_demotes_to_vacuous_on_read() {
    let legacy_forgery = r#"{"Held":{"evidence":""}}"#;
    let parsed: Verdict = serde_json::from_str(legacy_forgery).expect("legacy shape still parses");
    assert_eq!(
        parsed,
        Verdict::Vacuous {},
        "a legacy hold that witnessed nothing must not read back as held"
    );
    assert!(!parsed.held());
}

/// The legacy shape is accepted for reading only — nothing emits it any
/// more, so a round-trip normalizes onto the current tagged form.
#[test]
fn legacy_records_normalize_to_the_current_wire_shape_on_write() {
    let parsed: Verdict =
        serde_json::from_str(r#"{"Held":{"evidence":"probe saw a thing"}}"#).unwrap();
    let rewritten = serde_json::to_string(&parsed).unwrap();
    assert!(
        rewritten.contains("subjects"),
        "re-serialization must carry the witness, got {rewritten}"
    );
    assert!(
        !rewritten.contains("evidence"),
        "the legacy field must not survive a round-trip, got {rewritten}"
    );
}

// ── Clause 2 for the RUN-level verdict (the certification receipt) ─────

fn result(id: &str, verdict: Verdict) -> AssertionResult {
    AssertionResult {
        assertion_id: id.into(),
        kind: AssertionKind::CustomJob,
        verdict,
    }
}

fn held(id: &str) -> AssertionResult {
    result(
        id,
        Verdict::judge(
            Observations::scope(vec![Observation::new("ok")]),
            Vec::new(),
        ),
    )
}

fn input(preconditions: &[&str]) -> CertificationInput {
    CertificationInput {
        run_id: "derived-verdict-law".into(),
        scope: "trials".into(),
        precondition_ids: preconditions.iter().map(|s| (*s).to_string()).collect(),
        known_gaps: Vec::new(),
        at: "2026-07-27T00:00:00Z".into(),
    }
}

/// The run-level sibling of the same defect: a certification over ZERO
/// artifact assertions is `Inconclusive`, never `Certified`. ensaio's
/// classifier already guarded this with `!artifact_assertions.is_empty()`;
/// this pins it so a future refactor cannot silently drop the guard.
#[test]
fn certification_over_zero_artifact_assertions_is_inconclusive() {
    let receipt = CertificationReceipt::build(input(&[]), &[]);
    assert_eq!(receipt.verdict, CertVerdict::Inconclusive);
    assert_eq!(receipt.subject_count(), 0);
}

/// A certification over a non-empty artifact set that all held is
/// `Certified`, and the receipt publishes its subject count — the
/// subclass-E discipline: *a gate that cannot state what it examined cannot
/// be audited for what it missed.*
#[test]
fn certification_publishes_the_subject_count_it_examined() {
    let receipt =
        CertificationReceipt::build(input(&["pre"]), &[held("pre"), held("a"), held("b")]);
    assert_eq!(receipt.verdict, CertVerdict::Certified);
    assert_eq!(
        receipt.subject_count(),
        2,
        "the count is the ARTIFACT assertions certified, not the preconditions"
    );
}

/// A vacuous per-assertion verdict never contributes to a `Certified` run —
/// `Vacuous` is not `held()`, so the all-held test fails and the run routes
/// to `Inconclusive`, ensaio's existing honest verdict for "neither proven
/// nor disproven."
#[test]
fn a_vacuous_assertion_routes_the_certification_to_inconclusive() {
    let receipt = CertificationReceipt::build(
        input(&[]),
        &[held("a"), result("empty-corpus", Verdict::Vacuous {})],
    );
    assert_eq!(
        receipt.verdict,
        CertVerdict::Inconclusive,
        "a verdict over an empty corpus must not certify anything"
    );
}

/// A precondition that is `Vacuous` is an unmet precondition — it is not
/// `held()`, so it gates exactly like `Skipped`/`Errored`/`Falsified` and
/// routes every artifact to `Skipped` + `Inconclusive`.
#[test]
fn a_vacuous_precondition_gates_the_run() {
    let receipt = CertificationReceipt::build(
        input(&["pre"]),
        &[result("pre", Verdict::Vacuous {}), held("a")],
    );
    assert_eq!(receipt.verdict, CertVerdict::Inconclusive);
    assert!(matches!(
        receipt.artifact_assertions[0].verdict,
        Verdict::Skipped { .. }
    ));
}

// ── Clause 1: one classifier, and the closed arm set ──────────────────

/// `verdict_tag` (the content-address component) is exhaustive over the
/// closed arm set. This is subclass D's cure — no `_ =>` arm exists to round
/// `Vacuous` or `Unreached` up into a neighbouring class — and it is what
/// makes the receipt sha change when a verdict CLASS changes.
#[test]
fn every_verdict_arm_has_a_distinct_content_address_tag() {
    let arms = [
        Verdict::judge(Observations::scope(vec![Observation::new("x")]), Vec::new()),
        Verdict::judge(
            Observations::scope(vec![Observation::new("x")]),
            vec![Observation::new("y")],
        ),
        Verdict::Errored {
            message: "e".into(),
        },
        Verdict::Skipped { reason: "s".into() },
        Verdict::Vacuous {},
        Verdict::Unreached {},
    ];
    let tags: Vec<&str> = arms.iter().map(Verdict::tag).collect();
    let mut sorted = tags.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        tags.len(),
        "every arm needs its own tag, else two classes share a content address: {tags:?}"
    );
    assert_eq!(sorted.len(), 6, "the arm set is closed at six");
}

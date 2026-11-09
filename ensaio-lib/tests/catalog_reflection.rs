//! CATALOG REFLECTION — every `AssertionKind` variant has a matching
//! interpreter arm, or vice versa. Two independent checks, deliberately
//! layered:
//!
//! 1. `run_assertion`'s own `match` in `k8s_env.rs` is exhaustive over
//!    `AssertionKind` — Rust refuses to **compile** if a new variant
//!    is added without a matching arm. That's the real, load-bearing
//!    guarantee (truly-unrepresentable, not CI-caught).
//! 2. `AssertionKind::ALL` (the enumeration surface used for docs,
//!    tooling, and this test) is a hand-maintained array that Rust's
//!    exhaustiveness check does NOT protect on its own — a stale
//!    `ALL` missing a new variant still compiles fine. This file's
//!    `exhaustive_over_every_current_variant` function is the second
//!    check: an exhaustive match over a value of every kind in `ALL`,
//!    which fails to compile the moment a variant exists that isn't
//!    named in it.
//!
//! Together: (1) proves the *interpreter* can't silently ignore a
//! variant; (2) proves `ALL` can't silently drift stale.

use ensaio::AssertionKind;

/// If this fails to compile, `AssertionKind` grew a variant that
/// isn't named in this match — meaning it isn't guaranteed to be
/// named in `ALL` either. Add it here AND to `AssertionKind::ALL`
/// (`types.rs`) in the same change.
fn exhaustive_over_every_current_variant(k: AssertionKind) {
    match k {
        AssertionKind::EphemeralStorageOnly
        | AssertionKind::EnvironmentHealth
        | AssertionKind::AuthserviceAdmission
        | AssertionKind::TlsTerminationPassthrough
        | AssertionKind::HeaderSurvives
        | AssertionKind::NetworkPath
        | AssertionKind::CustomJob
        | AssertionKind::AmqpRoundtrip
        | AssertionKind::PitrDataRoundtrip
        | AssertionKind::RolloutSurgeBound
        | AssertionKind::LogWindow
        | AssertionKind::MetricInRange
        | AssertionKind::HpaBehavior
        | AssertionKind::ExecMetric
        | AssertionKind::ObjectFields
        | AssertionKind::WorkflowJobs
        | AssertionKind::StepsCompleted
        | AssertionKind::GuardHeld
        | AssertionKind::NaturalStateRestored => {}
    }
}

#[test]
fn every_variant_in_all_is_a_real_current_variant() {
    for kind in AssertionKind::ALL {
        exhaustive_over_every_current_variant(kind);
    }
}

#[test]
fn all_has_no_duplicates_and_the_canary_count_is_current() {
    // A stale AssertionKind::ALL (missing a newly-added variant) would
    // still compile — this canary count is the catch for that half of
    // the drift. Bump it in the SAME change that adds a variant.
    assert_eq!(
        AssertionKind::ALL.len(),
        19,
        "AssertionKind::ALL count drifted from the enum — update both"
    );

    let mut seen = std::collections::HashSet::new();
    for kind in AssertionKind::ALL {
        assert!(
            seen.insert(format!("{kind:?}")),
            "duplicate entry in AssertionKind::ALL: {kind:?}"
        );
    }
}

#[test]
fn every_kind_is_named_in_the_doc_catalog_table() {
    // One row per AssertionKind. This pins the exact variant names +
    // count any catalog table must match — a human (or agent) updating
    // such a table has this list to check against, and a variant rename
    // here is a visible diff against it.
    let names: Vec<String> = AssertionKind::ALL
        .iter()
        .map(|k| format!("{k:?}"))
        .collect();
    assert_eq!(
        names,
        vec![
            "EphemeralStorageOnly",
            "EnvironmentHealth",
            "AuthserviceAdmission",
            "TlsTerminationPassthrough",
            "HeaderSurvives",
            "NetworkPath",
            "CustomJob",
            "AmqpRoundtrip",
            "PitrDataRoundtrip",
            "RolloutSurgeBound",
            "LogWindow",
            "MetricInRange",
            "HpaBehavior",
            "ExecMetric",
            "ObjectFields",
            "WorkflowJobs",
            "StepsCompleted",
            "GuardHeld",
            "NaturalStateRestored"
        ]
    );
}

/// **The fifth face of the catalog, and the one that was missing.**
///
/// The four gates above all live in Rust: the interpreter's `match`, this
/// file's `match`, the `ALL` canary, the doc-table pin. None of them can see
/// `deploy/crds/ensaio-crd.yaml`, which carries the SAME closed set as a
/// structural-schema `enum` — and a structural schema is enforced by the
/// apiserver, so a kind absent from it cannot be declared on a cluster **at
/// all**.
///
/// That gap was not hypothetical. Measured on this branch: the committed CRD's
/// enum listed six kinds while the enum in `types.rs` had seven —
/// `EphemeralStorageOnly` had been added two commits earlier
/// (`6642055`) and the chart was never regenerated, and `targetNamespace`
/// (`9ad7b01`) was missing too. So the M1 environment-qualification work was
/// unusable against a real cluster: the apiserver would have **rejected** an
/// `EphemeralStorageOnly` assertion and silently **pruned** `targetNamespace`
/// off every other one — pruning being the worse half, since it fails by
/// quietly answering about the wrong namespace rather than by erroring.
///
/// Regenerate with `cargo run -p ensaio --bin ensaio -- print-crd >
/// deploy/crds/ensaio-crd.yaml`.
#[test]
fn the_committed_crd_enumerates_every_assertion_kind() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/crds/ensaio-crd.yaml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read the committed CRD at {}: {e}", path.display()));
    let doc: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&text).expect("the committed CRD is not valid YAML");

    let expected: Vec<String> = AssertionKind::ALL
        .iter()
        .map(|k| format!("{k:?}"))
        .collect();

    // Walk the whole document rather than hard-coding a path: the kind enum
    // appears once under `spec.assertions` and once under `status.results`, and
    // a navigation that named only one of them would gate only one of them.
    let mut sites: Vec<Vec<String>> = Vec::new();
    collect_assertion_kind_enums(&doc, &mut sites);

    // The DENOMINATOR, asserted before the contents. If the walk ever stops
    // finding these sites — a renamed key, a restructured schema, a file that
    // silently became empty — this test must fail loudly instead of passing
    // over an empty set.
    assert!(
        sites.len() >= 2,
        "expected at least 2 AssertionKind enum sites in the CRD \
         (spec.assertions + status.results), found {} — the walk found nothing \
         to check, which is a broken gate, not a pass",
        sites.len()
    );

    for site in &sites {
        assert_eq!(
            site, &expected,
            "the committed CRD's AssertionKind enum has drifted from `types.rs`. \
             Regenerate: cargo run -p ensaio --bin ensaio -- print-crd > \
             deploy/crds/ensaio-crd.yaml"
        );
    }
}

/// Every `enum:` sequence in the document that looks like the `AssertionKind`
/// set — identified by containing `CustomJob`, which every version of the enum
/// has carried since M0.
fn collect_assertion_kind_enums(v: &serde_yaml_ng::Value, out: &mut Vec<Vec<String>>) {
    match v {
        serde_yaml_ng::Value::Mapping(m) => {
            for (k, val) in m {
                if k.as_str() == Some("enum") {
                    if let Some(seq) = val.as_sequence() {
                        let names: Vec<String> = seq
                            .iter()
                            .filter_map(|e| e.as_str().map(str::to_owned))
                            .collect();
                        if names.iter().any(|n| n == "CustomJob") {
                            out.push(names);
                        }
                    }
                }
                collect_assertion_kind_enums(val, out);
            }
        }
        serde_yaml_ng::Value::Sequence(s) => {
            for e in s {
                collect_assertion_kind_enums(e, out);
            }
        }
        _ => {}
    }
}

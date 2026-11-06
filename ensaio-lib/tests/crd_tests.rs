//! Unit tests for the pure, client-free pieces of the M2 CRD/controller
//! layer. Full reconcile-loop integration testing (against a real or
//! `kind`-backed apiserver) is a named next increment — not faked here.

use ensaio::crd::EnsaioPhase;

#[test]
fn only_the_four_named_phases_are_terminal() {
    let all = [
        EnsaioPhase::Pending,
        EnsaioPhase::Provisioning,
        EnsaioPhase::Ready,
        EnsaioPhase::Wiring,
        EnsaioPhase::Asserting,
        EnsaioPhase::Settled,
        EnsaioPhase::Dormant,
        EnsaioPhase::Done,
        EnsaioPhase::Failed,
        EnsaioPhase::TtlExceeded,
        EnsaioPhase::Retired,
    ];
    let terminal: Vec<EnsaioPhase> = all.into_iter().filter(|p| p.is_terminal()).collect();
    assert_eq!(
        terminal,
        vec![
            EnsaioPhase::Done,
            EnsaioPhase::Failed,
            EnsaioPhase::TtlExceeded,
            EnsaioPhase::Retired
        ]
    );
}

#[test]
fn default_phase_is_pending() {
    assert_eq!(EnsaioPhase::default(), EnsaioPhase::Pending);
}

#[test]
fn crd_spec_converts_from_the_lisp_facing_spec() {
    use ensaio::crd::EnsaioCrdSpec;
    use ensaio::{EnsaioSpec, load_all};

    const SRC: &str = include_str!("../../specs/examples/admission-gate.ensaio.lisp");
    let spec = load_all::<EnsaioSpec>(SRC).unwrap().remove(0);
    let crd_spec = EnsaioCrdSpec::from_spec(&spec, 3);

    assert_eq!(crd_spec.scope, "trials");
    assert_eq!(crd_spec.systems.len(), 2);
    assert_eq!(crd_spec.assertions.len(), 1);
    assert_eq!(crd_spec.confirm_after_runs, 3);
}

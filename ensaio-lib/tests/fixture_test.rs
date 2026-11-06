//! Per-experiment fixture tests. The refusals matter most: a cycle must not be
//! broken arbitrarily, and residue must not be silent.

use ensaio::fixture::{
    Cleanup, Effect, Fixture, FixtureOrderError, cleanup_order, placement_order, residue,
};

fn fx(name: &str, after: &[&str], effect: Effect, cleanup: Cleanup) -> Fixture {
    Fixture {
        name: name.into(),
        effect,
        cleanup,
        after: after.iter().map(|s| (*s).to_string()).collect(),
        action: None,
        undo_phase: ensaio::fixture::UndoPhase::default(),
    }
}

fn create(name: &str, after: &[&str]) -> Fixture {
    fx(name, after, Effect::Create, Cleanup::EnvironmentIsDestroyed)
}

#[test]
fn dependencies_are_placed_before_their_dependents() {
    let f = vec![create("queue", &["policy"]), create("policy", &[])];
    let o = placement_order(&f).unwrap();
    assert_eq!(o[0].name, "policy");
    assert_eq!(o[1].name, "queue");
}

/// Cleanup reverses placement: undoing in declaration order can remove a
/// dependency out from under something still present.
#[test]
fn cleanup_is_the_reverse_of_placement() {
    let f = vec![create("policy", &[]), create("queue", &["policy"])];
    let c = cleanup_order(&f).unwrap();
    assert_eq!(c[0].name, "queue");
    assert_eq!(c[1].name, "policy");
}

/// A cycle is REFUSED, not broken. Picking an order silently would place data
/// in a sequence nobody declared and make the trial unreproducible in the one
/// way that matters.
#[test]
fn a_cycle_is_refused_and_names_the_fixtures_involved() {
    let f = vec![create("a", &["b"]), create("b", &["a"])];
    match placement_order(&f).expect_err("must refuse") {
        FixtureOrderError::Cycle { involved } => {
            assert_eq!(involved, vec!["a".to_string(), "b".to_string()]);
        }
        other => panic!("expected Cycle, got {other:?}"),
    }
}

/// An `after` pointing at nothing is a typo that would otherwise place the
/// fixture first and look fine.
#[test]
fn an_unknown_dependency_is_refused() {
    let f = vec![create("queue", &["polcy"])];
    match placement_order(&f).expect_err("must refuse") {
        FixtureOrderError::UnknownDependency { fixture, missing } => {
            assert_eq!(fixture, "queue");
            assert_eq!(missing, "polcy");
        }
        other => panic!("expected UnknownDependency, got {other:?}"),
    }
}

/// Duplicate names make `after` ambiguous, so they are refused rather than
/// resolved by position.
#[test]
fn duplicate_names_are_refused() {
    let f = vec![create("same", &[]), create("same", &[])];
    assert!(matches!(
        placement_order(&f),
        Err(FixtureOrderError::DuplicateName { .. })
    ));
}

/// Residue is reported, never silent — harmless in a destroyed environment,
/// the next trial's problem in a claimed one.
#[test]
fn retained_fixtures_are_reported_as_residue() {
    let f = vec![
        create("clean", &[]),
        fx(
            "left",
            &[],
            Effect::Create,
            Cleanup::Retained {
                reason: "the trial asserts on it afterwards".into(),
            },
        ),
    ];
    let r = residue(&f);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].name, "left");
    assert!(!f[0].leaves_residue());
}

/// A Mutate is NOT reversible even with an inverse: the inverse restores a
/// value someone chose, not the value that was there.
#[test]
fn a_mutation_is_not_reversible_even_with_an_inverse() {
    let inverse = create("restore", &[]);
    let m = fx(
        "tweak",
        &[],
        Effect::Mutate,
        Cleanup::Inverse(Box::new(inverse)),
    );
    assert!(!m.is_reversible());
    let c = create("make", &[]);
    assert!(c.is_reversible());
}

/// Declaration order is preserved where nothing constrains it, so a spec reads
/// the way it runs.
#[test]
fn independent_fixtures_keep_declaration_order() {
    let f = vec![create("one", &[]), create("two", &[]), create("three", &[])];
    let o = placement_order(&f).unwrap();
    let names: Vec<&str> = o.iter().map(|x| x.name.as_str()).collect();
    assert_eq!(names, vec!["one", "two", "three"]);
}

/// Existing specs have no fixtures and must keep working unchanged.
#[test]
fn an_empty_fixture_set_is_valid() {
    assert!(placement_order(&[]).unwrap().is_empty());
    assert!(residue(&[]).is_empty());
}

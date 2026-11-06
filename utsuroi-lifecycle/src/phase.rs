//! [`DomainPhase`] — the adapter every domain phase enum implements.
//!
//! Additive and non-invasive: a domain keeps its own rich, co-located phase
//! FSM (viveiro's `WarmEnvironmentPhase`, ensaio's `EnsaioPhase`, …) and only
//! *declares* the projection onto [`LifecycleStage`] plus its terminal set and
//! wire token. Because the projection lives in an `impl` in the domain crate
//! (orphan-rule: the domain owns the enum), the substrate never has to know
//! the domain's variants — it reasons over the shared stage only.
//!
//! A gift the trait carries: the `ALL` catalog-reflection const becomes part
//! of the contract, so kenshi and the tenant — which lack an inherent `ALL`
//! today — gain one for free the moment they implement the trait.

use crate::stage::LifecycleStage;

/// The projection contract a domain phase enum satisfies to join the shared
/// lifecycle spine.
///
/// `Copy` (which implies `Sized`) keeps the associated `ALL` const trivially
/// expressible as `&'static [Self]`.
pub trait DomainPhase: Copy + 'static {
    /// Every phase variant, once — the CATALOG-REFLECTION surface. A domain
    /// with an inherent `const ALL: [Self; N]` points this at it
    /// (`const ALL: &'static [Self] = &Self::ALL;`), so there is one catalog,
    /// not two.
    const ALL: &'static [Self];

    /// The shared projection — which universal [`LifecycleStage`] this domain
    /// phase occupies.
    fn stage(&self) -> LifecycleStage;

    /// Whether this phase is a terminal state of the domain FSM.
    fn is_terminal(&self) -> bool;

    /// The stable lower-kebab / NATS-subject token for this phase (typed
    /// rendering, never a `Debug` format).
    fn as_wire(&self) -> &'static str;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A local phase enum standing in for a domain FSM — proves the trait
    /// machinery (projection + `ALL` + terminals + wire token) without needing
    /// a real domain crate. The real domain projections
    /// (`WarmEnvironmentPhase` / `EnvironmentClaimPhase`) are proved against
    /// this trait in ensaio's own `tests/utsuroi_projection_test.rs`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum FakePhase {
        Queued,
        ComingUp,
        Live,
        Working,
        Gone,
    }

    impl FakePhase {
        const ALL_INHERENT: [FakePhase; 5] = [
            FakePhase::Queued,
            FakePhase::ComingUp,
            FakePhase::Live,
            FakePhase::Working,
            FakePhase::Gone,
        ];
    }

    impl DomainPhase for FakePhase {
        const ALL: &'static [Self] = &Self::ALL_INHERENT;

        fn stage(&self) -> LifecycleStage {
            match self {
                Self::Queued => LifecycleStage::Pending,
                Self::ComingUp => LifecycleStage::Spawning,
                Self::Live => LifecycleStage::Ready,
                Self::Working => LifecycleStage::Keeping,
                Self::Gone => LifecycleStage::Reaped,
            }
        }

        fn is_terminal(&self) -> bool {
            matches!(self, Self::Gone)
        }

        fn as_wire(&self) -> &'static str {
            match self {
                Self::Queued => "queued",
                Self::ComingUp => "coming-up",
                Self::Live => "live",
                Self::Working => "working",
                Self::Gone => "gone",
            }
        }
    }

    #[test]
    fn projection_maps_every_phase_to_a_stage() {
        assert_eq!(FakePhase::Queued.stage(), LifecycleStage::Pending);
        assert_eq!(FakePhase::ComingUp.stage(), LifecycleStage::Spawning);
        assert_eq!(FakePhase::Live.stage(), LifecycleStage::Ready);
        assert_eq!(FakePhase::Working.stage(), LifecycleStage::Keeping);
        assert_eq!(FakePhase::Gone.stage(), LifecycleStage::Reaped);
    }

    #[test]
    fn terminal_agrees_between_phase_and_its_projected_stage() {
        for p in FakePhase::ALL {
            // A terminal domain phase must project to a terminal stage. (The
            // converse need not hold: a non-terminal phase can still sit in a
            // terminal-ish stage in richer FSMs — but a terminal phase in a
            // non-terminal stage would be a projection bug.)
            if p.is_terminal() {
                assert!(
                    p.stage().is_terminal(),
                    "{p:?} is terminal but projects to non-terminal {:?}",
                    p.stage()
                );
            }
        }
    }

    #[test]
    fn all_const_is_the_single_catalog() {
        assert_eq!(FakePhase::ALL.len(), 5);
        assert_eq!(FakePhase::ALL, &FakePhase::ALL_INHERENT);
    }
}

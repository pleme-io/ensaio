//! [`SubMachine`] — the ORDERED sub-machine a [`LifecycleStage`] carries.
//!
//! # Why the flat spine was not enough
//!
//! [`LifecycleStage`] is deliberately coarse: seven stages every domain
//! projects onto. That coarseness is correct for reasoning ACROSS domains and
//! wrong for reasoning INSIDE one. Measured on viveiro before this module
//! existed: **eleven** distinct come-up states projected onto the single stage
//! `Spawning`. Wiring the flat spine to the environment controller in that
//! shape would have made the collapse permanent — the eleven states are
//! recoverable from inside `viveiro::stages`, and unrecoverable from anything
//! reading the projection.
//!
//! So a stage does not merely *have* a name; it may carry an ordered
//! sub-machine, and the projection carries the position within it. That is the
//! whole of this module: [`SubMachine`] declares the order, [`Nested`] is a
//! phase plus its position, and [`SubVerdict`] is what a gate answers.
//!
//! # The rule the order enforces
//!
//! Lifted from `viveiro::stages`, whose header states it exactly: **a stage may
//! not begin because an earlier one PROBABLY finished.** Order is carried by
//! the type (`Ord` from declaration order), not by a `Vec` a caller could
//! permute, so inserting a step in the wrong position changes every comparison
//! at once instead of one call site.
//!
//! # ★ The arm the flat spine had no shape for
//!
//! [`SubVerdict`] is FOUR-valued, and the fourth arm is the reason this module
//! is not just a generalization exercise.
//!
//! `viveiro::stages::StageVerdict` is three-valued — `Holds` / `Pending` /
//! `Errored` — and its own doc explains why two is wrong: "cannot tell" has to
//! be encoded as one of the others, and `Holds` admits on ignorance while
//! `Pending` waits out the budget on a fault that never clears.
//!
//! The same argument applied once more yields a fourth case that three cannot
//! express: a wait blocked on an **external clock**. A point-in-time restore
//! cannot begin until the source's restorable window opens; a maintenance
//! window is not open yet; a certificate is not yet valid. None of these is
//! `Pending` — nothing is converging and no amount of polling makes it true
//! sooner — and none is `Errored`, because nothing is wrong.
//!
//! Encoding a clock-wait as `Pending` is not a cosmetic loss. **`Pending`
//! consumes a failure budget and `AwaitingWindow` must not**, so the
//! conflation turns "the window opens in forty minutes" into "this stage
//! exhausted its retries". That is a real, recorded failure shape in the
//! restore-drill domain, which is precisely where the one existing
//! `AwaitingWindow`-shaped state lives today — stranded inside a domain FSM
//! with no spine arm to project onto.
//!
//! [`SubVerdict::is_budget_consuming`] is the discriminator, and it is the
//! method a controller should branch on rather than re-matching the enum.

use crate::stage::LifecycleStage;
use core::fmt;

/// An ordered sub-machine refining ONE [`LifecycleStage`].
///
/// A domain implements this for the enum of steps that occur *within* a stage.
/// `Copy` keeps [`SubMachine::ORDER`] trivially expressible as
/// `&'static [Self]`, matching [`crate::DomainPhase::ALL`].
///
/// Implementors get [`SubMachine::predecessor`], [`SubMachine::index`],
/// [`SubMachine::is_after`] and [`SubMachine::remaining`] derived from `ORDER`
/// alone — the sequence is declared once and every question about it is
/// answered from that one declaration.
pub trait SubMachine: Copy + PartialEq + 'static {
    /// Every step, in the order they must occur. The single source of the
    /// sequence — ★★ CATALOG REFLECTION, same contract as
    /// [`crate::DomainPhase::ALL`].
    const ORDER: &'static [Self];

    /// Which parent stage this sub-machine refines. A sub-machine belongs to
    /// exactly one stage; two stages wanting the same steps are two
    /// sub-machines, because their gates differ.
    fn parent_stage() -> LifecycleStage;

    /// Stable lower-kebab token for status messages and wire rendering
    /// (typed rendering, never a `Debug` format — ★★ TYPED EMISSION).
    fn name(self) -> &'static str;

    /// This step's position in [`SubMachine::ORDER`].
    ///
    /// Returns `None` only if an implementor omits a variant from `ORDER`,
    /// which [`assert_order_is_total`] exists to catch in a test.
    #[must_use]
    fn index(self) -> Option<usize> {
        Self::ORDER.iter().position(|s| *s == self)
    }

    /// The step that must hold before this one may begin — derived from
    /// `ORDER`, never hand-written, so it cannot disagree with the sequence.
    #[must_use]
    fn predecessor(self) -> Option<Self> {
        match self.index() {
            Some(0) | None => None,
            Some(i) => Self::ORDER.get(i - 1).copied(),
        }
    }

    /// Whether `self` occurs strictly after `other` in the sequence.
    #[must_use]
    fn is_after(self, other: Self) -> bool {
        match (self.index(), other.index()) {
            (Some(a), Some(b)) => a > b,
            _ => false,
        }
    }

    /// The steps still ahead of `self`, in order — what a status surface shows
    /// as "remaining", computed rather than restated.
    #[must_use]
    fn remaining(self) -> &'static [Self] {
        match self.index() {
            Some(i) => &Self::ORDER[i + 1..],
            None => &[],
        }
    }
}

/// Assert a [`SubMachine`]'s `ORDER` is TOTAL over the variants a caller
/// passes, and free of duplicates.
///
/// The anti-vacuity half: a `SubMachine` whose `ORDER` omits a variant answers
/// `predecessor()` with `None` for it — which reads exactly like "this is the
/// first step" and admits it out of order. That is a silent wrong answer, so
/// it gets a test helper rather than a doc note. Pass the domain's own
/// exhaustive variant list (usually its inherent `ALL`).
///
/// # Panics
/// Panics naming the offending variant when `ORDER` has a duplicate, omits a
/// variant of `all`, or carries one `all` does not.
pub fn assert_order_is_total<S: SubMachine + fmt::Debug>(all: &[S]) {
    let mut seen: Vec<&'static str> = Vec::new();
    for s in S::ORDER {
        assert!(
            !seen.contains(&s.name()),
            "SubMachine::ORDER lists {s:?} twice — the sequence is ambiguous"
        );
        seen.push(s.name());
    }
    assert_eq!(
        S::ORDER.len(),
        all.len(),
        "SubMachine::ORDER has {} steps but the domain declares {} variants — \
         a variant missing from ORDER answers predecessor() with None, which \
         reads as 'first step' and admits it out of order",
        S::ORDER.len(),
        all.len()
    );
    for v in all {
        assert!(
            v.index().is_some(),
            "{v:?} is a variant of the sub-machine but is absent from ORDER"
        );
    }
}

/// Whether a sub-machine step's PROPERTY holds right now.
///
/// Four-valued. See this module's header for why three was not enough; the
/// short form: `AwaitingWindow` is a wait on a clock nobody controls, and it
/// must not consume the failure budget that `Pending` does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubVerdict {
    /// The property holds. The next step may begin.
    Holds,
    /// Not yet true, and converging — keep polling. **Consumes budget.**
    Pending {
        /// What is still missing, for a status surface.
        reason: String,
    },
    /// Blocked on an EXTERNAL CLOCK. Nothing is wrong and nothing is
    /// converging; polling sooner does not make it true sooner.
    /// **Does not consume budget.**
    AwaitingWindow {
        /// What clock is being waited on (a restorable window opening, a
        /// maintenance window, a not-before). Named, so a status surface can
        /// say *which* clock rather than "waiting".
        clock: String,
        /// Seconds until the window is expected to open, when the source
        /// reports it. `None` when the source will only say "not yet" —
        /// which is a real answer and is not rounded to a guess.
        opens_in_seconds: Option<u64>,
    },
    /// The gate could not be evaluated — a fault that will not clear by
    /// waiting. Held AND explained, never admitted on ignorance.
    Errored {
        /// The fault, for a status surface.
        detail: String,
    },
}

impl SubVerdict {
    /// Whether this verdict should count against a retry / failure budget.
    ///
    /// **This is the method to branch on**, not a re-`match` at each call site:
    /// the whole point of the fourth arm is that one of the three waiting-ish
    /// verdicts is free, and a call site that re-derives that rule will
    /// eventually get it wrong.
    #[must_use]
    pub fn is_budget_consuming(&self) -> bool {
        match self {
            Self::Pending { .. } | Self::Errored { .. } => true,
            Self::Holds | Self::AwaitingWindow { .. } => false,
        }
    }

    /// Whether the sub-machine may advance past this step.
    #[must_use]
    pub fn advances(&self) -> bool {
        matches!(self, Self::Holds)
    }

    /// A stable lower-kebab token per arm (typed rendering).
    #[must_use]
    pub fn as_wire(&self) -> &'static str {
        match self {
            Self::Holds => "holds",
            Self::Pending { .. } => "pending",
            Self::AwaitingWindow { .. } => "awaiting-window",
            Self::Errored { .. } => "errored",
        }
    }
}

/// A domain phase PLUS its position in the sub-machine refining that phase's
/// stage — the value that stops a rich come-up FSM collapsing into one stage.
///
/// `stage()` still answers the coarse cross-domain question; `step` keeps the
/// fine one answerable, so a projection is no longer lossy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nested<P, S> {
    /// The domain phase.
    pub phase: P,
    /// Where inside the parent stage's sub-machine this phase sits. `None`
    /// for a phase occupying a stage that carries no sub-machine.
    pub step: Option<S>,
}

impl<P: crate::DomainPhase, S: SubMachine> Nested<P, S> {
    /// A phase with a position in its stage's sub-machine.
    #[must_use]
    pub fn at(phase: P, step: S) -> Self {
        Self {
            phase,
            step: Some(step),
        }
    }

    /// A phase in a stage that carries no sub-machine.
    #[must_use]
    pub fn bare(phase: P) -> Self {
        Self { phase, step: None }
    }

    /// The coarse cross-domain projection — unchanged, so every existing
    /// consumer of the flat spine keeps working.
    #[must_use]
    pub fn stage(&self) -> LifecycleStage {
        self.phase.stage()
    }

    /// Whether the `step` (if any) actually belongs to the sub-machine
    /// refining this phase's stage.
    ///
    /// A `Nested` carrying a step from a DIFFERENT stage's sub-machine is a
    /// nonsense value the type system cannot currently forbid (both are just
    /// `S`), so it is checkable here and asserted in tests. Named honestly:
    /// this is **only-mitigated**, not unrepresentable.
    #[must_use]
    pub fn step_belongs_to_stage(&self) -> bool {
        self.step.is_none() || S::parent_stage() == self.phase.stage()
    }

    /// A typed `stage/step` token — `"spawning/model"`, or just `"ready"` when
    /// the stage carries no sub-machine.
    #[must_use]
    pub fn as_wire(&self) -> String {
        match self.step {
            Some(s) => {
                let mut out = String::with_capacity(24);
                out.push_str(self.stage().as_str());
                out.push('/');
                out.push_str(s.name());
                out
            }
            None => self.stage().as_str().to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phase::DomainPhase;

    /// A come-up sub-machine standing in for viveiro's five-stage chain.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ComeUp {
        Topology,
        Config,
        Model,
        Bootstrap,
        Qualify,
    }

    impl ComeUp {
        const ALL: [ComeUp; 5] = [
            ComeUp::Topology,
            ComeUp::Config,
            ComeUp::Model,
            ComeUp::Bootstrap,
            ComeUp::Qualify,
        ];
    }

    impl SubMachine for ComeUp {
        const ORDER: &'static [ComeUp] = &ComeUp::ALL;
        fn parent_stage() -> LifecycleStage {
            LifecycleStage::Spawning
        }
        fn name(self) -> &'static str {
            match self {
                ComeUp::Topology => "topology",
                ComeUp::Config => "config",
                ComeUp::Model => "model",
                ComeUp::Bootstrap => "bootstrap",
                ComeUp::Qualify => "qualify",
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum FakePhase {
        Warming,
        Ready,
    }

    impl DomainPhase for FakePhase {
        const ALL: &'static [FakePhase] = &[FakePhase::Warming, FakePhase::Ready];
        fn stage(&self) -> LifecycleStage {
            match self {
                FakePhase::Warming => LifecycleStage::Spawning,
                FakePhase::Ready => LifecycleStage::Ready,
            }
        }
        fn is_terminal(&self) -> bool {
            false
        }
        fn as_wire(&self) -> &'static str {
            match self {
                FakePhase::Warming => "warming",
                FakePhase::Ready => "ready",
            }
        }
    }

    #[test]
    fn order_is_total_and_duplicate_free() {
        assert_order_is_total(&ComeUp::ALL);
    }

    #[test]
    fn predecessor_is_derived_from_order_not_hand_written() {
        assert_eq!(ComeUp::Topology.predecessor(), None);
        assert_eq!(ComeUp::Config.predecessor(), Some(ComeUp::Topology));
        assert_eq!(ComeUp::Model.predecessor(), Some(ComeUp::Config));
        assert_eq!(ComeUp::Bootstrap.predecessor(), Some(ComeUp::Model));
        assert_eq!(ComeUp::Qualify.predecessor(), Some(ComeUp::Bootstrap));
    }

    #[test]
    fn is_after_and_remaining_agree_with_order() {
        assert!(ComeUp::Qualify.is_after(ComeUp::Topology));
        assert!(!ComeUp::Topology.is_after(ComeUp::Qualify));
        // Not after itself — the comparison is strict.
        assert!(!ComeUp::Model.is_after(ComeUp::Model));
        assert_eq!(
            ComeUp::Model.remaining(),
            &[ComeUp::Bootstrap, ComeUp::Qualify]
        );
        assert_eq!(ComeUp::Qualify.remaining(), &[] as &[ComeUp]);
    }

    /// ★ The load-bearing test: the fourth arm exists so a clock-wait does not
    /// burn a failure budget. If this ever goes green with `AwaitingWindow`
    /// counted as consuming, the arm has become decorative.
    #[test]
    fn awaiting_window_does_not_consume_budget_but_pending_does() {
        let pending = SubVerdict::Pending {
            reason: "13 of 17 workloads ready".into(),
        };
        let window = SubVerdict::AwaitingWindow {
            clock: "rds restorable window".into(),
            opens_in_seconds: Some(2400),
        };
        let errored = SubVerdict::Errored {
            detail: "403 from the qualification endpoint".into(),
        };

        assert!(pending.is_budget_consuming());
        assert!(errored.is_budget_consuming());
        assert!(
            !window.is_budget_consuming(),
            "a wait on an external clock must not exhaust the retry budget — \
             that conflation turns 'opens in 40 minutes' into 'out of retries'"
        );
        // And none of the three waiting verdicts advances the machine.
        assert!(!pending.advances());
        assert!(!window.advances());
        assert!(!errored.advances());
        assert!(SubVerdict::Holds.advances());
    }

    #[test]
    fn awaiting_window_may_decline_to_guess_an_eta() {
        // A source that will only say "not yet" is a real answer; it is not
        // rounded up to a fabricated ETA.
        let w = SubVerdict::AwaitingWindow {
            clock: "certificate not-before".into(),
            opens_in_seconds: None,
        };
        assert!(!w.is_budget_consuming());
        assert_eq!(w.as_wire(), "awaiting-window");
    }

    #[test]
    fn every_verdict_arm_has_a_distinct_wire_token() {
        let arms = [
            SubVerdict::Holds,
            SubVerdict::Pending {
                reason: String::new(),
            },
            SubVerdict::AwaitingWindow {
                clock: String::new(),
                opens_in_seconds: None,
            },
            SubVerdict::Errored {
                detail: String::new(),
            },
        ];
        let mut seen = std::collections::HashSet::new();
        for a in &arms {
            assert!(
                seen.insert(a.as_wire()),
                "duplicate wire token {}",
                a.as_wire()
            );
        }
        assert_eq!(seen.len(), 4);
    }

    /// ★ The reason this module exists: the projection is no longer lossy.
    #[test]
    fn nesting_keeps_the_sub_position_the_flat_projection_would_lose() {
        let a = Nested::at(FakePhase::Warming, ComeUp::Config);
        let b = Nested::at(FakePhase::Warming, ComeUp::Bootstrap);

        // The coarse projection cannot tell them apart — that is the collapse.
        assert_eq!(a.stage(), b.stage());
        assert_eq!(a.stage(), LifecycleStage::Spawning);

        // The nested value can, and says so on the wire.
        assert_ne!(a, b);
        assert_eq!(a.as_wire(), "spawning/config");
        assert_eq!(b.as_wire(), "spawning/bootstrap");
        assert!(b.step.unwrap().is_after(a.step.unwrap()));
    }

    #[test]
    fn a_stage_without_a_sub_machine_renders_bare() {
        let n: Nested<FakePhase, ComeUp> = Nested::bare(FakePhase::Ready);
        assert_eq!(n.as_wire(), "ready");
        assert_eq!(n.stage(), LifecycleStage::Ready);
        assert!(n.step_belongs_to_stage());
    }

    /// Only-mitigated, and labelled as such: a step from another stage's
    /// sub-machine is a constructible nonsense value, caught by a check rather
    /// than made unrepresentable.
    #[test]
    fn a_step_from_the_wrong_stage_is_detected() {
        let ok = Nested::at(FakePhase::Warming, ComeUp::Model);
        assert!(ok.step_belongs_to_stage());

        // ComeUp refines Spawning, but this phase is in Ready.
        let wrong = Nested::at(FakePhase::Ready, ComeUp::Model);
        assert!(
            !wrong.step_belongs_to_stage(),
            "a step belonging to another stage's sub-machine must be detectable"
        );
    }
}

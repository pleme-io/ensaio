//! [`LifecycleStage`] — the universal skeleton every domain phase FSM projects
//! into. THIS is the shared mechanism (not a unified phase enum).
//!
//! Grounded in one stage table: viveiro's `Warming/Ready/
//! Claimed/Draining/Reaped`, ensaio's 11-variant FSM, kenshi's per-run arc,
//! and the tenant's `InfraProvisioning/…/Reaped` all collapse onto the same
//! seven stages — while each keeps its own rich enum. `Keeping` is where every
//! domain's semantics legitimately diverge (asserting / testing / claimed /
//! scanning); the substrate reasons over the stage, never the domain middle.

use serde::{Deserialize, Serialize};

/// The seven-stage lifecycle skeleton every [`crate::DomainPhase`] projects
/// onto. `Reaped` and `Failed` are the two terminal stages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LifecycleStage {
    /// Pre-spawn / queued — declared but nothing provisioned yet.
    Pending,
    /// Coming up — provisioning the isolated footprint.
    Spawning,
    /// Provisioned and reachable (note: *provisioned*, not necessarily
    /// *waited-healthy* — that distinction is a domain concern).
    Ready,
    /// The domain does its work here (assert / test / hold-a-claim / scan) and
    /// the TTL runs. Every domain's semantics live in this stage and diverge.
    Keeping,
    /// Being torn down — reaping the footprint.
    Draining,
    /// Terminal-good: the footprint is gone.
    Reaped,
    /// Terminal-bad: failed / timed-out / retired.
    Failed,
}

impl LifecycleStage {
    /// Every stage, once — the CATALOG-REFLECTION enumeration surface,
    /// co-located with the enum so a drift test can pin it.
    pub const ALL: [LifecycleStage; 7] = [
        LifecycleStage::Pending,
        LifecycleStage::Spawning,
        LifecycleStage::Ready,
        LifecycleStage::Keeping,
        LifecycleStage::Draining,
        LifecycleStage::Reaped,
        LifecycleStage::Failed,
    ];

    /// The two terminal stages — `Reaped` (good) and `Failed` (bad).
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Reaped | Self::Failed)
    }

    /// A stable, typed lower-kebab token per stage (typed rendering, never a
    /// `format!("{:?}")` of `Debug` output).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Spawning => "spawning",
            Self::Ready => "ready",
            Self::Keeping => "keeping",
            Self::Draining => "draining",
            Self::Reaped => "reaped",
            Self::Failed => "failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_covers_every_stage_once() {
        assert_eq!(LifecycleStage::ALL.len(), 7);
        // No duplicates: each stage maps to a unique wire token.
        let mut seen = std::collections::HashSet::new();
        for s in LifecycleStage::ALL {
            assert!(seen.insert(s.as_str()), "duplicate wire token for {s:?}");
        }
    }

    #[test]
    fn exactly_two_terminals() {
        let terminals: Vec<_> = LifecycleStage::ALL
            .into_iter()
            .filter(|s| s.is_terminal())
            .collect();
        assert_eq!(
            terminals,
            vec![LifecycleStage::Reaped, LifecycleStage::Failed]
        );
    }

    #[test]
    fn wire_tokens_are_lower_kebab_and_round_trip_through_serde() {
        let s = LifecycleStage::Keeping;
        assert_eq!(s.as_str(), "keeping");
        let json = serde_json::to_string(&s).unwrap();
        let back: LifecycleStage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }
}
